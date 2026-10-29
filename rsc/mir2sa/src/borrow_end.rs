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
//! Phase 4 - loop back-edge ends: a borrow created inside a loop and
//! dead before a back-edge (not live-out of the back-edge pred under
//! use-before-def liveness) is ended at the back-edge pred, before its
//! Goto into the loop head, so the next iteration's merge sees the
//! source unlocked. The canonical shape is a poll loop (09): the
//! future pin is borrowed fresh each iteration and the borrow must
//! not survive the back-edge, or the head merge sees Locked_Read from
//! the loop path against Active from the entry path (PhiStateConflict).
//! Gates are conservative: loop head with >=2 preds, a Goto back-edge
//! pred reachable from the head, single definition inside the loop
//! whose defining block dominates the back-edge pred (else the end
//! would free an unbound reg on an entry path that bypasses the def:
//! 23's head-defined borrow reached through a pre-header back-edge),
//! never MIR-dropped, no version conflict, not phase-3-ended (and not
//! borrowing a phase-3-ended reg), not used by the back-edge
//! terminator, not live-out of the back-edge pred, the source's whole
//! borrower closure passing together, and no phase-1 end site
//! reachable across the back-edge without an intervening redefinition
//! (else double-free). Loop ends are kept separate from merge_ends
//! and are NOT folded into `merged`: a loop end serves only the
//! back-edge path, so a downstream Drop on the exit path still needs
//! (and keeps) its own phase-1 end. The exit side is coordinated in
//! drop.rs: exit_frees keeps a free for a reg freed on a back-edge
//! and redefined before the exit (stale-may analysis), so the loop
//! end never starves the exit. Anything outside this shape is left
//! untouched - no loud mark, no new trap.
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
    /// Block index -> borrows to free at a loop back-edge end, leaf
    /// first, before a Goto into a loop head (phase 4).
    pub loop_ends: BTreeMap<usize, Vec<String>>,
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

/// Places a statement's rvalue reads.
fn stmt_places(st: &Stmt) -> Vec<&str> {
    match st {
        Stmt::Assign { rvalue, .. } => match rvalue {
            Rvalue::Use { op } => operand_place(op).into_iter().collect(),
            Rvalue::Repeat { op, .. } => operand_place(op).into_iter().collect(),
            Rvalue::Call { args, .. } => args.iter().filter_map(operand_place).collect(),
            Rvalue::BinOp { left, right, .. } => {
                [left.as_ref(), right.as_ref()].iter().filter_map(|o| operand_place(o)).collect()
            }
            Rvalue::UnOp { operand, .. } => operand_place(operand).into_iter().collect(),
            Rvalue::Cast { op, .. } => operand_place(op).into_iter().collect(),
            Rvalue::Aggregate { elems, .. } => elems.iter().filter_map(operand_place).collect(),
            Rvalue::Ref { place, .. } => vec![place.as_str()],
            Rvalue::Discriminant { place } | Rvalue::RawPtr { place, .. } => vec![place.as_str()],
            _ => vec![],
        },
        Stmt::SetDisc { place, .. } => vec![place.as_str()],
        _ => vec![],
    }
}

/// Place a statement defines, if any.
fn stmt_def(st: &Stmt) -> Option<&str> {
    if let Stmt::Assign { dest, .. } = st {
        Some(dest.as_str())
    } else {
        None
    }
}

/// Places a terminator reads.
fn term_places(t: &Term) -> Vec<&str> {
    match t {
        Term::Call { args, .. } => args.iter().filter_map(operand_place).collect(),
        Term::Drop { place, .. } => vec![place.as_str()],
        Term::SwitchInt { discr, .. } => operand_place(discr).into_iter().collect(),
        Term::Assert { cond, .. } => operand_place(cond).into_iter().collect(),
        Term::InlineAsm { ins, .. } => ins.iter().filter_map(operand_place).collect(),
        Term::Return { ret } => ret.as_deref().into_iter().collect(),
        _ => vec![],
    }
}

/// Places a terminator defines.
fn term_defs(t: &Term) -> Vec<&str> {
    match t {
        Term::Call { dest, .. } => dest.as_deref().into_iter().collect(),
        Term::InlineAsm { outs, .. } => outs.iter().map(|s| s.as_str()).collect(),
        _ => vec![],
    }
}

/// Plan loop back-edge ends (phase 4; see module docs). Returns block
/// index -> regs to free at the block end, leaf first, before a Goto
/// into a loop head. Pure MIR-level.
///
/// A borrow dead before a back-edge pred BE (not live-out of BE under
/// use-before-def liveness) whose source's whole borrower closure passes
/// the gates is ended in BE, so the next iteration's merge at the head
/// sees the source unlocked. Kept separate from merge_ends: a loop end
/// serves only the back-edge path, so a downstream Drop on the exit
/// path still needs (and keeps) its own phase-1 end. Guards: a loop end
/// yields to any phase-1 end reachable across it without redefinition
/// (else double-free), and to another back-edge end of the same reg
/// reachable without redefinition. The defining block must dominate BE:
/// with a single definition, any entry path to BE bypassing the def
/// would free an unbound register (UnknownRegister, corpus 23).
fn plan_loop_ends(
    blocks: &[Block],
    succ: &[Vec<usize>],
    edges: &[(String, String, usize)],
    defs: &BTreeMap<String, Vec<usize>>,
    drop_at: &BTreeMap<String, Vec<usize>>,
    conflicted: &BTreeSet<String>,
    entry_reach: &HashSet<usize>,
    doms: &[HashSet<usize>],
    merged: &BTreeSet<String>,
    phase1_ends: &BTreeMap<usize, Vec<String>>,
) -> BTreeMap<usize, Vec<String>> {
    let n = blocks.len();
    let mut preds: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for (i, ss) in succ.iter().enumerate() {
        for &s in ss {
            preds[s].insert(i);
        }
    }
    // Use-before-def liveness (statement order; terminator uses come
    // after all statements; terminator defs count as block defs).
    let mut uses_before_def: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    let mut defs_in_block: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    for (bi, b) in blocks.iter().enumerate() {
        let mut defined: BTreeSet<String> = BTreeSet::new();
        for st in &b.statements {
            for p in stmt_places(st) {
                if !defined.contains(p) {
                    uses_before_def[bi].insert(p.to_string());
                }
            }
            if let Some(d) = stmt_def(st) {
                defined.insert(d.to_string());
                defs_in_block[bi].insert(d.to_string());
            }
        }
        for p in term_places(&b.terminator) {
            if !defined.contains(p) {
                uses_before_def[bi].insert(p.to_string());
            }
        }
        for d in term_defs(&b.terminator) {
            defs_in_block[bi].insert(d.to_string());
        }
    }
    let mut live_in: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    let mut live_out: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    loop {
        let mut changed = false;
        for bi in (0..n).rev() {
            let mut out: BTreeSet<String> = BTreeSet::new();
            for &s in &succ[bi] {
                out.extend(live_in[s].iter().cloned());
            }
            if out != live_out[bi] {
                live_out[bi] = out;
                changed = true;
            }
            let mut inn = uses_before_def[bi].clone();
            for r in live_out[bi].iter() {
                if !defs_in_block[bi].contains(r) {
                    inn.insert(r.clone());
                }
            }
            if inn != live_in[bi] {
                live_in[bi] = inn;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // Borrower graph: source -> direct borrowers; borrower -> source.
    let mut borrowers_of: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut source_of: BTreeMap<String, String> = BTreeMap::new();
    for (b, s, _) in edges {
        borrowers_of.entry(s.clone()).or_default().insert(b.clone());
        source_of.insert(b.clone(), s.clone());
    }
    // Reachability from `from`, optionally skipping one block (used to
    // test "without an intervening redefinition": the def block is the
    // skipped node, so a path through it does not count).
    let reach = |from: usize, skip: Option<usize>| -> HashSet<usize> {
        let mut seen = HashSet::new();
        let mut stack = vec![from];
        seen.insert(from);
        while let Some(i) = stack.pop() {
            for &j in &succ[i] {
                if Some(j) == skip {
                    continue;
                }
                if seen.insert(j) {
                    stack.push(j);
                }
            }
        }
        seen
    };
    let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for hi in 0..n {
        if !entry_reach.contains(&hi) || preds[hi].len() < 2 {
            continue;
        }
        let reach_from_h = reach(hi, None);
        for &be in &preds[hi] {
            if be == hi || !entry_reach.contains(&be) || !reach_from_h.contains(&be) {
                continue;
            }
            if !matches!(&blocks[be].terminator, Term::Goto { target } if target == &blocks[hi].id) {
                continue;
            }
            let in_loop = |db: usize| -> bool {
                db == hi || (reach_from_h.contains(&db) && reach(db, None).contains(&hi))
            };
            let mut be_ends: BTreeSet<String> = BTreeSet::new();
            let sources: BTreeSet<String> = borrowers_of.keys().cloned().collect();
            for p in &sources {
                let mut closure: BTreeSet<String> = BTreeSet::new();
                let mut stack: Vec<String> =
                    borrowers_of.get(p).map(|s| s.iter().cloned().collect()).unwrap_or_default();
                let mut cyclic = false;
                while let Some(b) = stack.pop() {
                    if b == *p {
                        cyclic = true;
                        continue;
                    }
                    if !closure.insert(b.clone()) {
                        continue;
                    }
                    if let Some(bs) = borrowers_of.get(&b) {
                        for nb in bs {
                            stack.push(nb.clone());
                        }
                    }
                }
                if cyclic || closure.is_empty() {
                    continue;
                }
                let mut ok = true;
                for b in &closure {
                    let db = defs.get(b).and_then(|v| v.first().copied());
                    let single = defs.get(b).is_some_and(|v| v.len() == 1);
                    let db_in_loop = db.is_some_and(in_loop);
                    let db_dominates_be = db.is_some_and(|d| doms[be].contains(&d));
                    let no_drop = drop_at.get(b).is_none_or(|v| v.is_empty());
                    let no_conflict = !conflicted.contains(b);
                    let not_merged = !merged.contains(b);
                    let src_not_merged = source_of.get(b).is_none_or(|s| !merged.contains(s));
                    let not_live_out = !live_out[be].contains(b);
                    let no_term_use = !term_uses(&blocks[be].terminator, b);
                    if !(single && db_in_loop && db_dominates_be && no_drop && no_conflict
                        && not_merged && src_not_merged && not_live_out && no_term_use)
                    {
                        ok = false;
                        break;
                    }
                    let db = db.unwrap();
                    for (dblk, regs) in phase1_ends {
                        if !regs.contains(b) {
                            continue;
                        }
                        if reach(*dblk, Some(db)).contains(&be)
                            || reach(be, Some(db)).contains(dblk)
                        {
                            ok = false;
                            break;
                        }
                    }
                    if !ok {
                        break;
                    }
                }
                if !ok {
                    continue;
                }
                for b in closure {
                    be_ends.insert(b);
                }
            }
            if !be_ends.is_empty() {
                out.insert(be, be_ends.into_iter().collect());
            }
        }
    }
    // Pairwise guard: a reg ended at two back-edges double-frees if one
    // end block reaches the other without passing the reg's def block.
    let mut by_reg: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (be, regs) in &out {
        for r in regs {
            by_reg.entry(r.clone()).or_default().push(*be);
        }
    }
    for (r, bes) in by_reg {
        if bes.len() < 2 {
            continue;
        }
        let Some(db) = defs.get(&r).and_then(|v| v.first().copied()) else {
            continue;
        };
        let mut veto = false;
        for (i, a) in bes.iter().enumerate() {
            for b in bes.iter().skip(i + 1) {
                if reach(*a, Some(db)).contains(b) || reach(*b, Some(db)).contains(a) {
                    veto = true;
                }
            }
        }
        if veto {
            for be in bes {
                if let Some(v) = out.get_mut(&be) {
                    v.retain(|x| x != &r);
                }
            }
        }
    }
    out.retain(|_, v| !v.is_empty());
    // Leaf-first order within each block (a borrower before its source).
    for regs in out.values_mut() {
        let mut ordered: Vec<String> = vec![];
        let mut pending: BTreeSet<String> = regs.iter().cloned().collect();
        loop {
            let ready: Vec<String> = pending
                .iter()
                .filter(|r| {
                    borrowers_of.get(*r).is_none_or(|bs| bs.iter().all(|b| !pending.contains(b)))
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
        // A cycle (impossible in versioned MIR) would stall: drop the
        // stalled tail rather than emit an unsound order.
        *regs = ordered;
    }
    out.retain(|_, v| !v.is_empty());
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
    let mut plan = DropPlan { ends: BTreeMap::new(), loud: BTreeSet::new(), cleanup: BTreeSet::new(), merge_ends, loop_ends: BTreeMap::new() };
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
    // Loop back-edge ends (phase 4) run after phase 1 so the double-end
    // guard can see phase-1 end sites; they are deliberately NOT added
    // to `merged` (a loop end serves only the back-edge path, so a
    // downstream Drop on the exit path still needs its own end).
    let phase1_ends = plan.ends.clone();
    plan.loop_ends = plan_loop_ends(
        blocks, &succ, &edges, &defs, &drop_at, &conflicted, &entry_reach, doms, &merged,
        &phase1_ends,
    );
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

    fn ca(dest: &str, func: &str, args: Vec<&str>) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: None,
            rvalue: Rvalue::Call {
                func: func.to_string(),
                args: args.into_iter().map(|a| Operand::Copy { place: a.to_string() }).collect(),
                sig: None,
            },
        }
    }

    fn ct(func: &str, args: Vec<&str>, dest: Option<&str>, target: Option<&str>) -> Term {
        Term::Call {
            func: func.to_string(),
            func_raw: None,
            args: args.into_iter().map(|a| Operand::Copy { place: a.to_string() }).collect(),
            dest: dest.map(|d| d.to_string()),
            target: target.map(|t| t.to_string()),
            sig: None,
        }
    }

    // 09-shape poll loop. bb0: _4 = pin(_5) -> bb1; bb1: _6 = waker -> bb2;
    // bb2 (head): _11 = &_4; _10 = as_mut(_11) -> bb3; bb3: _12 = &_6;
    // _9 = poll(_10, _12) -> bb4; bb4: switch _9 -> bb5 (ready) / bb6
    // (pending) / otherwise bb7; bb5: Drop _4 -> bb8; bb6: yield -> bb9;
    // bb9: goto bb2 (back-edge); bb7: goto bb8; bb8: return.
    fn poll_loop() -> Vec<Block> {
        vec![
            blk("bb0", vec![], ct("pin", vec!["_5"], Some("_4"), Some("bb1"))),
            blk("bb1", vec![], ct("waker", vec![], Some("_6"), Some("bb2"))),
            blk("bb2", vec![rf("_11", "_4"), ca("_10", "as_mut", vec!["_11"])],
                Term::Goto { target: "bb3".into() }),
            blk("bb3", vec![rf("_12", "_6"), ca("_9", "poll", vec!["_10", "_12"])],
                Term::Goto { target: "bb4".into() }),
            blk("bb4", vec![], Term::SwitchInt {
                discr: Box::new(Operand::Copy { place: "_9".to_string() }),
                targets: vec![("0".to_string(), "bb5".to_string()), ("1".to_string(), "bb6".to_string())],
                otherwise: "bb7".to_string(),
            }),
            blk("bb5", vec![], Term::Drop { place: "_4".to_string(), target: "bb8".to_string() }),
            blk("bb6", vec![], ct("yield", vec![], Some("_8"), Some("bb9"))),
            blk("bb7", vec![], Term::Goto { target: "bb8".to_string() }),
            blk("bb8", vec![], Term::Return { ret: None }),
            blk("bb9", vec![], Term::Goto { target: "bb2".to_string() }),
        ]
    }

    #[test]
    fn loop_backedge_ends_poll_shape() {
        let blocks = poll_loop();
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert_eq!(plan.loop_ends.get(&9), Some(&vec!["_11".to_string(), "_12".to_string()]));
        // The exit-path Drop keeps its own end: loop ends serve only
        // the back-edge path and never starve the exit.
        assert_eq!(plan.ends.get(&5), Some(&vec!["_11".to_string()]));
        assert!(plan.loud.is_empty(), "loud: {:?}", plan.loud);
        assert!(plan.merge_ends.is_empty());
    }

    #[test]
    fn loop_backedge_live_out_vetoes() {
        let mut blocks = poll_loop();
        // Use _11 before its def in the head: the borrow is carried
        // across iterations, so it is live-out of the back-edge pred.
        blocks[2] = blk("bb2", vec![ca("_10", "as_mut", vec!["_11"]), rf("_11", "_4")],
            Term::Goto { target: "bb3".into() });
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert_eq!(plan.loop_ends.get(&9), Some(&vec!["_12".to_string()]));
        assert_eq!(plan.ends.get(&5), Some(&vec!["_11".to_string()]));
    }

    #[test]
    fn loop_backedge_dropped_borrower_vetoes() {
        let mut blocks = poll_loop();
        blocks[6] = blk("bb6", vec![], Term::Drop { place: "_11".to_string(), target: "bb9".to_string() });
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert_eq!(plan.loop_ends.get(&9), Some(&vec!["_12".to_string()]));
        assert!(plan.loud.contains(&5), "Drop _4 is loud once _11 is MIR-dropped");
    }

    #[test]
    fn loop_backedge_non_goto_skipped() {
        let mut blocks = poll_loop();
        blocks[9] = blk("bb9", vec![], ct("yield", vec![], None, Some("bb2")));
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.loop_ends.is_empty(), "loop_ends: {:?}", plan.loop_ends);
    }

    #[test]
    fn loop_backedge_self_loop_skipped() {
        let blocks = vec![
            blk("bb0", vec![rf("_1", "_2")], Term::Goto { target: "bb0".into() }),
            blk("bb1", vec![], Term::Goto { target: "bb0".into() }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.loop_ends.is_empty(), "loop_ends: {:?}", plan.loop_ends);
    }

    #[test]
    fn loop_backedge_phase1_guard_vetoes() {
        // Direct plan_loop_ends call: a fabricated phase-1 end at bb6
        // (Drop _4 on a path into the back-edge pred) must veto _11's
        // loop end, while _12 (no phase-1 end) is still ended.
        let mut blocks = poll_loop();
        blocks[6] = blk("bb6", vec![], Term::Drop { place: "_4".to_string(), target: "bb9".to_string() });
        let succ = successors(&blocks);
        let edges = vec![("_11".to_string(), "_4".to_string(), 2usize), ("_12".to_string(), "_6".to_string(), 3usize)];
        let mut defs: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        defs.insert("_11".to_string(), vec![2]);
        defs.insert("_12".to_string(), vec![3]);
        let entry_reach: HashSet<usize> = (0..blocks.len()).collect();
        let merged: BTreeSet<String> = BTreeSet::new();
        let mut phase1_ends: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        phase1_ends.insert(6, vec!["_11".to_string()]);
        let ends = plan_loop_ends(&blocks, &succ, &edges, &defs, &BTreeMap::new(), &BTreeSet::new(), &entry_reach, &doms(&blocks), &merged, &phase1_ends);
        assert_eq!(ends.get(&9), Some(&vec!["_12".to_string()]));
        // Control: without the phase-1 end, both borrows end at the back-edge.
        let ends = plan_loop_ends(&blocks, &succ, &edges, &defs, &BTreeMap::new(), &BTreeSet::new(), &entry_reach, &doms(&blocks), &merged, &BTreeMap::new());
        assert_eq!(ends.get(&9), Some(&vec!["_11".to_string(), "_12".to_string()]));
    }

    #[test]
    fn loop_backedge_def_must_dominate_backedge() {
        // 23-shape: _13 = &_11 is defined in the head bb2, but the
        // back-edge pred bb4 is reachable from the entry (bb0) without
        // passing bb2, so ending _13 in bb4 would free an unbound reg
        // on the first iteration (UnknownRegister). No loop end.
        let blocks = vec![
            blk("bb0", vec![], Term::SwitchInt {
                discr: Box::new(Operand::Copy { place: "_c".to_string() }),
                targets: vec![("0".to_string(), "bb2".to_string())],
                otherwise: "bb4".to_string(),
            }),
            blk("bb2", vec![rf("_13", "_11"), ca("_12", "next", vec!["_13"])],
                Term::Goto { target: "bb3".into() }),
            blk("bb3", vec![], Term::SwitchInt {
                discr: Box::new(Operand::Copy { place: "_12".to_string() }),
                targets: vec![("0".to_string(), "bb4".to_string())],
                otherwise: "bb5".to_string(),
            }),
            blk("bb4", vec![], Term::Goto { target: "bb2".into() }),
            blk("bb5", vec![], Term::Return { ret: None }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks));
        assert!(plan.loop_ends.is_empty(), "loop_ends: {:?}", plan.loop_ends);
    }
}
