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
//! Phase 5 - join ends (phi-11). Two shapes the earlier phases miss:
//! (a) Def-block ends: a borrower used only inside its defining block B
//! (statements, or a Call terminator's args) still locks its source
//! textually on every path out of B, so any join fed by B sees the source
//! Locked_Read against Active from a bypass path (308: borrows made and
//! consumed by one block's call, joining into a Return block, which
//! phase 3 skips; 108/23: borrows made and consumed by a loop-head
//! block's call, where phase 4 only handles Goto back-edges and 108's
//! whole-closure gate also fails on an exit-path borrower). Ending the
//! borrower at the end of B (after its Call, before B's jmp) unlocks the
//! source on all paths out of B at once. Gates mirror phase 4's caution:
//! single definition in B, never MIR-dropped, unconflicted, not already
//! ended by phases 1/3/4, never itself borrowed (chains stay with phase
//! 3), all uses inside B, no terminator use except a Call's args, and no
//! other borrower of the source live-out of B.
//! (b) Join-consume ends: a source moved on some arms of a join but not
//! others (Copy rendered as a move on one arm, a real Move on one arm)
//! disagrees at the join (03/25/314/39/105/30/333). When the source is
//! dead after the join, never ended elsewhere, and the join dominates
//! every reachable Return block (so no bypass path can leak it), the
//! Active arms get a move-into-dummy pair (`_phi_<arm>_<s> = s` then
//! `!_phi_<arm>_<s>`) at their end and every arm reaches the join
//! Consumed. A bare `!s` does NOT normalize a block-defined local at a
//! join (sa-check probe: local `!` arms still read Active, param `!`
//! arms read Consumed); a move does, on both. Arm states are
//! classified, never guessed: Consumed iff a consuming event
//! (plain-assign Move/Copy of an unspilled source, a Drop of the
//! source, or an earlier end of it) sits in a block dominating the
//! arm's end; Active iff no consuming event can reach the arm's end
//! and every borrower of the source is either never bound on a path
//! there or ended on every path there; anything else vetoes the whole
//! source. Only Goto, Call and (non-reading) Assert arms can take an
//! end: a SwitchInt arm would free the source on its sibling arms too.
//! An end lands on an arm only when the source is bound on every path
//! to that arm's end (its single def block dominates the arm, or it is
//! a header-bound param): ending an unbound register traps
//! UnknownRegister (103: `!_3` in bb0, whose def sits in bb1).
//! Spill-covered sources never count plain-assign events (their Copy
//! uses reload from a slot and never consume the original).
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
    /// Phase-5 ends for non-Call blocks, emitted before the terminator
    /// (same point as merge_ends): def-block borrower ends first, then
    /// join-consume ends, deduplicated.
    pub block_ends: BTreeMap<usize, Vec<JoinEnd>>,
    /// Phase-5 ends for Call blocks, emitted after the call and before
    /// its jmp (lower.rs injects them at the drop-glue point so drop.rs
    /// still sees them as ordinary frees).
    pub call_ends: BTreeMap<usize, Vec<JoinEnd>>,
    /// Statement-level place-store conflict ends (02): (block, stmt)
    /// -> (before, after). `before` ends borrows that would conflict
    /// with a direct mut-borrow store; `after` ends the store's own
    /// borrow when it dies at the store. Lower applies them only when
    /// the store actually renders as a store.
    pub store_ends: BTreeMap<(usize, usize), (Vec<String>, Vec<String>)>,
}

/// One phase-5 end line-group at a block end. `Free` is a 5a borrower
/// end (`!b`); `MoveFree` is a 5b join-consume end, a move into a fresh
/// dummy plus an immediate free of the dummy (see the module docs: a
/// bare `!s` does not normalize a block-defined local at a join).
/// `reg()` is the register whose state the end consumes (the borrower
/// for `Free`, the source for `MoveFree`); `lines()` is the SA text,
/// borrower ends first (variant order) and, among move-frees, borrower
/// sources before their sources (topo order in plan_join_ends).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum JoinEnd {
    Free(String),
    MoveFree { dummy: String, src: String },
}

impl JoinEnd {
    pub fn reg(&self) -> &str {
        match self {
            JoinEnd::Free(r) => r,
            JoinEnd::MoveFree { src, .. } => src,
        }
    }
    pub fn lines(&self) -> Vec<String> {
        match self {
            JoinEnd::Free(r) => vec![format!("    !{}", r)],
            JoinEnd::MoveFree { dummy, src } => {
                vec![format!("    {} = {}", dummy, src), format!("    !{}", dummy)]
            }
        }
    }
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
        Operand::Move { place , .. } | Operand::Copy { place , .. } | Operand::Conflict { place } => {
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
/// Statement-level place-store conflict ends (02).
///
/// A direct mut-borrow store `(*B) = v` writes through borrower `B`
/// of source `S`. Any OTHER borrow of `S` still live at the store
/// traps ReadWriteConflict in SA (its NLL lifetime should have ended
/// at its last use, but borrow_end otherwise only ends at block
/// ends). And `B` itself must end right after the store, or a later
/// shared borrow of `S` conflicts with the still-live mut borrow.
///
/// Conservative gates (anything else keeps today's behavior):
/// - the store place is exactly `(*B)` (no field/index steps);
/// - `B` is a non-ZST mut borrow of a plain local `S`;
/// - each ended borrow is defined before the store (same block
///   earlier, or a dominating block) and has no use outside the
///   store's block before the store (before-ends) or no use after
///   the store anywhere (after-end for `B`).
/// Uses cover statement operands, Ref/RawPtr places, terminator
/// operands, Drop places, and dest_place bases of other stores.
fn plan_store_ends(
    blocks: &[Block],
    doms: &[HashSet<usize>],
) -> BTreeMap<(usize, usize), (Vec<String>, Vec<String>)> {
    fn is_plain_local(s: &str) -> bool {
        s.len() > 1 && s.starts_with('_') && s[1..].chars().all(|c| c.is_ascii_digit())
    }
    fn base_of_place(dp: &str) -> Option<String> {
        // First `_N` token in a dest_place text (`(*_4)`, `(_1.1)`).
        let bytes = dp.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'_' {
                let mut j = i + 1;
                while j < bytes.len() && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                if j > i + 1 {
                    return Some(dp[i..j].to_string());
                }
            }
            i += 1;
        }
        None
    }
    fn op_uses(op: &Operand, bi: usize, si: usize, uses: &mut BTreeMap<String, Vec<(usize, usize)>>) {
        if let Some(p) = operand_place(op) {
            uses.entry(p.to_string()).or_default().push((bi, si));
        }
    }
    // Ref defs: borrower -> (source, mut, def block, def stmt).
    let mut ref_defs: BTreeMap<String, (String, bool, usize, usize)> = BTreeMap::new();
    // Fine uses: local -> positions (terminators use usize::MAX).
    let mut uses: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    // Store statements: (block, stmt, base borrow).
    let mut stores: Vec<(usize, usize, String)> = vec![];
    for (bi, b) in blocks.iter().enumerate() {
        for (si, st) in b.statements.iter().enumerate() {
            match st {
                Stmt::Assign { dest, dest_place, rvalue } => {
                    match rvalue {
                        Rvalue::Use { op } => op_uses(op, bi, si, &mut uses),
                        Rvalue::BinOp { left, right, .. } => {
                            op_uses(left, bi, si, &mut uses);
                            op_uses(right, bi, si, &mut uses);
                        }
                        Rvalue::Cast { op, .. } => op_uses(op, bi, si, &mut uses),
                        Rvalue::Aggregate { elems, .. } => {
                            for e in elems {
                                op_uses(e, bi, si, &mut uses);
                            }
                        }
                        Rvalue::Repeat { op, .. } => op_uses(op, bi, si, &mut uses),
                        Rvalue::Ref { place, mut_, zst, .. } => {
                            uses.entry(place.clone()).or_default().push((bi, si));
                            if !*zst {
                                ref_defs
                                    .entry(dest.clone())
                                    .or_insert((place.clone(), *mut_, bi, si));
                            }
                        }
                        Rvalue::RawPtr { place, .. } => {
                            uses.entry(place.clone()).or_default().push((bi, si));
                        }
                        _ => {}
                    }
                    if let Some(dp) = dest_place {
                        // Same versioned-block rule as def counting:
                        // a stale plain-local dest_place is a normal
                        // def, not a store.
                        let t = dp.trim();
                        let is_store = !(t.len() > 1
                            && t.starts_with('_')
                            && t[1..]
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '_'))
                            && base_of_place(dp).is_some();
                        if is_store {
                            let base = base_of_place(dp).unwrap();
                            uses.entry(base.clone()).or_default().push((bi, si));
                            stores.push((bi, si, base));
                        }
                    }
                }
                Stmt::SetDisc { place, .. } => {
                    uses.entry(place.clone()).or_default().push((bi, si));
                }
                _ => {}
            }
        }
        match &b.terminator {
            Term::Call { args, .. } => {
                for a in args {
                    op_uses(a, bi, usize::MAX, &mut uses);
                }
            }
            Term::SwitchInt { discr, .. } => {
                op_uses(discr, bi, usize::MAX, &mut uses);
            }
            Term::Assert { cond, .. } => {
                op_uses(cond, bi, usize::MAX, &mut uses);
            }
            Term::Drop { place, .. } => {
                uses.entry(place.clone()).or_default().push((bi, usize::MAX));
            }
            Term::Return { ret } => {
                if let Some(r) = ret {
                    uses.entry(r.clone()).or_default().push((bi, usize::MAX));
                }
            }
            Term::InlineAsm { ins, .. } => {
                for a in ins {
                    op_uses(a, bi, usize::MAX, &mut uses);
                }
            }
            _ => {}
        }
    }
    let def_ok = |dbi: usize, dsi: usize, bi: usize, si: usize, doms: &[HashSet<usize>]| {
        (dbi == bi && dsi < si) || (dbi != bi && doms.get(bi).is_some_and(|d| d.contains(&dbi)))
    };
    let mut out: BTreeMap<(usize, usize), (Vec<String>, Vec<String>)> = BTreeMap::new();
    for (bi, si, _base) in &stores {
        let b = &blocks[*bi];
        let Some(Stmt::Assign { dest_place: Some(dp), .. }) = b.statements.get(*si) else {
            continue;
        };
        // Direct borrow only: exactly `(*B)`.
        let Some(inner) = dp.trim().strip_prefix("(*").and_then(|s| s.strip_suffix(')')) else {
            continue;
        };
        if !is_plain_local(inner) {
            continue;
        }
        let store_base = inner.to_string();
        let Some((src, is_mut, dbi, dsi)) = ref_defs.get(&store_base) else {
            continue;
        };
        if !*is_mut || !is_plain_local(src) {
            continue;
        }
        if !def_ok(*dbi, *dsi, *bi, *si, doms) {
            continue;
        }
        let mut before: Vec<String> = vec![];
        for (other, (osrc, _, odbi, odsi)) in &ref_defs {
            if other == &store_base || osrc != src {
                continue;
            }
            if !def_ok(*odbi, *odsi, *bi, *si, doms) {
                continue;
            }
            let dead_before = uses
                .get(other)
                .is_none_or(|v| v.iter().all(|&(bj, sj)| bj == *bi && sj < *si));
            if dead_before {
                before.push(other.clone());
            }
        }
        let mut after: Vec<String> = vec![];
        let base_dead_after = uses.get(&store_base).is_none_or(|v| {
            v.iter().all(|&(bj, sj)| bj == *bi && sj <= *si)
        });
        if base_dead_after {
            after.push(store_base.clone());
        }
        if !before.is_empty() || !after.is_empty() {
            out.insert((*bi, *si), (before, after));
        }
    }
    out
}

pub fn plan_drops(blocks: &[Block], doms: &[HashSet<usize>], spilled: &BTreeSet<String>) -> DropPlan {
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
            if let Stmt::Assign { dest, dest_place, rvalue, .. } = st {
                // Mirror version.rs, adapted to VERSIONED blocks: a
                // normal def's dest_place is stale (dest=_1_v1,
                // dest_place=_1), so only a structured place counts
                // as a store (see spill.rs def_count for why).
                let is_store = dest_place.as_deref().is_some_and(|dp| {
                    let t = dp.trim();
                    !(t.len() > 1
                        && t.starts_with('_')
                        && t[1..]
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_'))
                });
                if !is_store {
                    defs.entry(dest.clone()).or_default().push(bi);
                }
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
    let mut plan = DropPlan { ends: BTreeMap::new(), loud: BTreeSet::new(), cleanup: BTreeSet::new(), merge_ends, loop_ends: BTreeMap::new(), block_ends: BTreeMap::new(), call_ends: BTreeMap::new(), store_ends: BTreeMap::new() };
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
    // Join ends (phase 5) run last so their vetoes can see every earlier
    // end: a reg ended anywhere else is never ended or consumed again.
    let (block_ends, call_ends, subsumed_loop) = plan_join_ends(
        blocks,
        &succ,
        &edges,
        &defs,
        &uses,
        &drop_at,
        &conflicted,
        &entry_reach,
        doms,
        &plan.merge_ends,
        &phase1_ends,
        &plan.loop_ends,
        spilled,
    );
    plan.block_ends = block_ends;
    plan.call_ends = call_ends;
    // 5a def-block ends subsume phase-4 loop ends for the same borrower
    // (every path to a loop end dominated by the def block passes the
    // def-block end; keeping both would double-free on the body path).
    // Prune the subsumed regs from the loop ends before emission.
    if !subsumed_loop.is_empty() {
        for v in plan.loop_ends.values_mut() {
            v.retain(|r| !subsumed_loop.contains(r));
        }
        plan.loop_ends.retain(|_, v| !v.is_empty());
    }
    // Statement-level store conflict ends (computed last: independent
    // of the other phases; lower suppresses a later block-end end for
    // a reg already ended here, so no double free).
    plan.store_ends = plan_store_ends(blocks, doms);
    plan
}

/// Use-before-def liveness over versioned MIR (same shape as phase 4's
/// local computation, kept separate so phase 4 stays untouched).
/// Returns (live_in, live_out) per block.
fn compute_live(
    blocks: &[Block],
    succ: &[Vec<usize>],
) -> (Vec<BTreeSet<String>>, Vec<BTreeSet<String>>) {
    let n = blocks.len();
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
    (live_in, live_out)
}

/// Blocks that can reach `to` (reverse-graph BFS, including `to` itself).
fn ancestors_of(succ: &[Vec<usize>], to: usize) -> HashSet<usize> {
    let n = succ.len();
    let mut pred: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ss) in succ.iter().enumerate() {
        for &j in ss {
            pred[j].push(i);
        }
    }
    let mut seen: HashSet<usize> = HashSet::from([to]);
    let mut stack = vec![to];
    while let Some(i) = stack.pop() {
        for &p in &pred[i] {
            if seen.insert(p) {
                stack.push(p);
            }
        }
    }
    seen
}

/// True for plain MIR local names (`_N`, digits only after the `_`).
/// Versioned names (`_4_v1`) and synthetic temps (`_sw_*`, `_mv_*`) fail.
fn is_local_name(reg: &str) -> bool {
    reg.len() > 1
        && reg.starts_with('_')
        && reg[1..].chars().all(|ch| ch.is_ascii_digit())
}

/// Plan phase-5 join ends (see module docs). Returns (block_ends,
/// call_ends): non-Call blocks take their ends before the terminator,
/// Call blocks after the call and before the jmp.
#[allow(clippy::too_many_arguments)]
fn plan_join_ends(
    blocks: &[Block],
    succ: &[Vec<usize>],
    edges: &[(String, String, usize)],
    defs: &BTreeMap<String, Vec<usize>>,
    uses: &BTreeMap<String, BTreeSet<usize>>,
    drop_at: &BTreeMap<String, Vec<usize>>,
    conflicted: &BTreeSet<String>,
    entry_reach: &HashSet<usize>,
    doms: &[HashSet<usize>],
    merge_ends: &BTreeMap<usize, Vec<String>>,
    phase1_ends: &BTreeMap<usize, Vec<String>>,
    loop_ends: &BTreeMap<usize, Vec<String>>,
    spilled: &BTreeSet<String>,
) -> (BTreeMap<usize, Vec<JoinEnd>>, BTreeMap<usize, Vec<JoinEnd>>, BTreeSet<String>) {
    let n = blocks.len();
    let (live_in, live_out) = compute_live(blocks, succ);
    let mut preds: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for (i, ss) in succ.iter().enumerate() {
        for &s in ss {
            preds[s].insert(i);
        }
    }
    let borrowers_of = |src: &str| -> BTreeSet<String> {
        edges.iter().filter(|(_, s, _)| s == src).map(|(b, _, _)| b.clone()).collect()
    };
    // Borrower graph for the borrowed-by check (chains stay with phase 3).
    let mut borrowed_by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (b, s, _) in edges {
        borrowed_by.entry(s.clone()).or_default().insert(b.clone());
    }
    let mut block_ends: BTreeMap<usize, Vec<JoinEnd>> = BTreeMap::new();
    let mut call_ends: BTreeMap<usize, Vec<JoinEnd>> = BTreeMap::new();
    let mut push_end = |bi: usize, r: &str, is_call: bool, block_ends: &mut BTreeMap<usize, Vec<JoinEnd>>, call_ends: &mut BTreeMap<usize, Vec<JoinEnd>>| {
        let m = if is_call { call_ends } else { block_ends };
        let v = m.entry(bi).or_default();
        if !v.iter().any(|x| x.reg() == r) {
            v.push(JoinEnd::Free(r.to_string()));
        }
    };
    // 5b move-free push: a fresh dummy per (block, source); skipped when
    // the source already has an end at this block (5a first).
    let push_move_free = |bi: usize, src: &str, dummy: String, is_call: bool, block_ends: &mut BTreeMap<usize, Vec<JoinEnd>>, call_ends: &mut BTreeMap<usize, Vec<JoinEnd>>| {
        let m = if is_call { call_ends } else { block_ends };
        let v = m.entry(bi).or_default();
        if !v.iter().any(|x| x.reg() == src) {
            v.push(JoinEnd::MoveFree { dummy, src: src.to_string() });
        }
    };
    // ---- Phase 5a: def-block borrower ends. ----
    let mut five_a: BTreeSet<String> = BTreeSet::new();
    let mut subsumed_loop: BTreeSet<String> = BTreeSet::new();
    let phase1_regs: BTreeSet<String> =
        phase1_ends.values().flatten().cloned().collect();
    let loop_regs: BTreeSet<String> = loop_ends.values().flatten().cloned().collect();
    let merged_regs: BTreeSet<String> = merge_ends.values().flatten().cloned().collect();
    for (b, p, db) in edges {
        if b == p || !entry_reach.contains(db) {
            continue;
        }
        if !defs.get(b).is_some_and(|v| v.len() == 1 && v[0] == *db) {
            continue;
        }
        if conflicted.contains(b) || drop_at.get(b).is_some_and(|v| !v.is_empty()) {
            continue;
        }
        if merged_regs.contains(b) || phase1_regs.contains(b) {
            continue;
        }
        // A phase-4 loop end does not veto 5a when every loop end for
        // this borrower sits in a block dominated by the def block (23:
        // _13's loop end is at bb14, dominated by its def block bb10):
        // the def-block end covers every path the loop end covered, plus
        // the loop-exit path the loop end misses (bb11 -> bb3, where _11
        // stayed Locked_Read). The loop end is then pruned by the caller
        // (keeping both would double-free on the body path).
        let mut subsumed_loop_here = false;
        if loop_regs.contains(b) {
            let dominated = loop_ends.iter().all(|(bi, regs)| {
                !regs.iter().any(|x| x == b) || doms[*bi].contains(db)
            });
            if !dominated {
                continue;
            }
            subsumed_loop_here = true;
        }
        // Chains stay with phase 3: never end a borrower that is itself
        // borrowed (freeing it while its borrower is live would trap).
        if borrowed_by.contains_key(b) {
            continue;
        }
        // All uses inside the defining block (block-level uses include
        // terminator uses; the terminator gate below refines that).
        if uses.get(b).is_none_or(|us| us.iter().any(|u| *u != *db)) {
            continue;
        }
        // The borrower must still be live at the end of its def block:
        // a statement inside the block that moves it (plain Use move,
        // single-elem aggregate move, or RawPtr derivation — all render
        // as a plain SA move) already consumed it, and ending it again
        // would trap (120: _3 moved into _2, then 5a freed _3). Call
        // args (statement or terminator) are reads in SA, not moves,
        // so they do not veto; nor do Cast/BinOp/Discriminant reads
        // or multi-elem aggregate stores.
        let moved_in_def = blocks[*db].statements.iter().any(|st| match st {
            Stmt::Assign { rvalue, .. } => match rvalue {
                // Field-precision: projected moves/borrows render
                // as field loads/adds and never consume the base, so
                // they do not veto the 5a end (plain forms still do).
                Rvalue::Use { op: Operand::Move { place, off, pty, .. } } if *off == 0 && pty.as_deref() != Some("buf") => place == b,
                Rvalue::Aggregate { elems, .. } if elems.len() == 1 => {
                    matches!(&elems[0], Operand::Move { place, off, pty, .. } if *off == 0 && pty.as_deref() != Some("buf") && place == b)
                }
                Rvalue::RawPtr { place, off, pty, .. } if *off == 0 && pty.as_deref() != Some("buf") => place == b,
                _ => false,
            },
            _ => false,
        });
        if moved_in_def {
            continue;
        }
        let is_call = match &blocks[*db].terminator {
            Term::Call { target: Some(_), .. } => true,
            Term::Goto { .. } | Term::SwitchInt { .. } | Term::Assert { .. } => {
                if term_uses(&blocks[*db].terminator, b) {
                    continue;
                }
                false
            }
            _ => continue,
        };
        // No other borrower of the source may still be live at the end
        // of the defining block (it would keep the source locked, and
        // the end would not unlock anything it needs to).
        if borrowers_of(p).iter().any(|b2| b2 != b && live_out[*db].contains(b2)) {
            continue;
        }
        five_a.insert(b.clone());
        if subsumed_loop_here {
            subsumed_loop.insert(b.clone());
        }
        push_end(*db, b, is_call, &mut block_ends, &mut call_ends);
    }
    // ---- Phase 5b: join-consume ends. ----
    // Consuming events per reg: plain-assign Move/Copy (Use, single-elem
    // Aggregate, Cast operands), Drop terminators, and every earlier
    // end of the reg itself (phase 1/3/4/5a). Stores, call args, binop
    // operands and eq reads never consume.
    let mut events: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut add_event = |r: &str, bi: usize, events: &mut BTreeMap<String, Vec<usize>>| {
        let v = events.entry(r.to_string()).or_default();
        if !v.contains(&bi) {
            v.push(bi);
        }
    };
    for (bi, b) in blocks.iter().enumerate() {
        for st in &b.statements {
            // Field-precision: a FIELD read loads from a field
            // cell and never consumes the base, so it is not a
            // consuming event for the base (terminator reads are
            // handled separately by term_uses and stay ungated: an end
            // must not be emitted before a terminator reads the field).
            // Pure deref reads still count (313: excluding them lost
            // the deref base's slot and the join conflicted).
            let ops: Vec<&Operand> = match st {
                Stmt::Assign { rvalue, .. } => match rvalue {
                    Rvalue::Use { op } => vec![op],
                    Rvalue::Aggregate { elems, .. } if elems.len() == 1 => elems.iter().collect(),
                    Rvalue::Cast { op, .. } => vec![op.as_ref()],
                    _ => vec![],
                },
                _ => vec![],
            };
            for op in ops {
                if crate::render_util::operand_is_field_projected(op) {
                    continue;
                }
                let Some(p) = operand_place(op) else {
                    continue;
                };
                // Spill-covered sources never consume on a plain
                // assign: their Copy uses reload from a slot (03: the
                // old code counted reloads as consumes and freed the
                // source on the wrong arm, manufacturing a conflict).
                if spilled.contains(p) {
                    continue;
                }
                add_event(p, bi, &mut events);
            }
        }
        if let Term::Drop { place, .. } = &b.terminator {
            if place != "__VERSION_CONFLICT__" {
                add_event(place, bi, &mut events);
            }
        }
    }
    for (bi, regs) in merge_ends.iter().chain(phase1_ends.iter()).chain(loop_ends.iter()) {
        for r in regs {
            add_event(r, *bi, &mut events);
        }
    }
    for (bi, regs) in &block_ends {
        for r in regs {
            add_event(r.reg(), *bi, &mut events);
        }
    }
    for (bi, regs) in &call_ends {
        for r in regs {
            add_event(r.reg(), *bi, &mut events);
        }
    }
    let ends_at_dominating = |m: &BTreeMap<usize, Vec<String>>, r: &str, p: usize| -> bool {
        m.iter().any(|(bi, regs)| regs.iter().any(|x| x == r) && doms[p].contains(bi))
    };
    // Return blocks (entry-reachable) for the no-bypass-leak gate.
    let returns: Vec<usize> = (0..n)
        .filter(|bi| {
            entry_reach.contains(bi) && matches!(blocks[*bi].terminator, Term::Return { .. })
        })
        .collect();
    // Candidate sources: single-def regs and header-bound params.
    let mut candidates: BTreeSet<String> = BTreeSet::new();
    for r in defs.keys().chain(uses.keys()) {
        if r == "__VERSION_CONFLICT__" || conflicted.contains(r) {
            continue;
        }
        if phase1_regs.contains(r) || loop_regs.contains(r) || merged_regs.contains(r) || five_a.contains(r) {
            continue;
        }
        candidates.insert(r.clone());
    }
    for j in 0..n {
        if !entry_reach.contains(&j) {
            continue;
        }
        let jpreds: Vec<usize> =
            preds[j].iter().copied().filter(|p| entry_reach.contains(p)).collect();
        if jpreds.len() < 2 {
            continue;
        }
        // No bypass path may leak the source past the join: every
        // reachable Return block must be dominated by the join.
        if returns.iter().any(|r| !doms[*r].contains(&j)) {
            continue;
        }
        for s in &candidates {
            // Defined once and dominating the join, or header-bound.
            let dominated = match defs.get(s) {
                Some(v) => v.len() == 1 && doms[j].contains(&v[0]),
                None => uses.contains_key(s) && is_local_name(s),
            };
            if !dominated {
                continue;
            }
            // Dead after the join (uses and Drops past it count as live).
            if live_in[j].contains(s) {
                continue;
            }
            // Classify every predecessor's end state; any doubt vetoes.
            enum St {
                Active,
                Consumed,
            }
            let mut states: BTreeMap<usize, St> = BTreeMap::new();
            let mut veto = false;
            for &p in &jpreds {
                let evs = events.get(s).cloned().unwrap_or_default();
                if evs.iter().any(|e| doms[p].contains(e)) {
                    states.insert(p, St::Consumed);
                    continue;
                }
                if evs.iter().any(|e| ancestors_of(succ, p).contains(e)) {
                    veto = true;
                    break;
                }
                // Active only if every borrower is absent on paths to p
                // or ended on every path to p.
                let mut borrowers_ok = true;
                for b in borrowers_of(s) {
                    let Some(db) = defs.get(&b).filter(|v| v.len() == 1).map(|v| v[0]) else {
                        borrowers_ok = false;
                        break;
                    };
                    if !ancestors_of(succ, p).contains(&db) {
                        continue;
                    }
                    let ended_everywhere = (block_ends
                        .get(&db)
                        .is_some_and(|v| v.iter().any(|x| x.reg() == b))
                        || call_ends
                            .get(&db)
                            .is_some_and(|v| v.iter().any(|x| x.reg() == b))
                        || merge_ends
                            .get(&db)
                            .is_some_and(|v| v.iter().any(|x| x == &b)))
                        && doms[p].contains(&db);
                    let ended_by_drop = ends_at_dominating(phase1_ends, &b, p)
                        || ends_at_dominating(loop_ends, &b, p);
                    if !ended_everywhere && !ended_by_drop {
                        borrowers_ok = false;
                        break;
                    }
                }
                if !borrowers_ok {
                    veto = true;
                    break;
                }
                states.insert(p, St::Active);
            }
            if veto {
                continue;
            }
            let n_active = states.values().filter(|st| matches!(st, St::Active)).count();
            let n_consumed = states.values().filter(|st| matches!(st, St::Consumed)).count();
            if n_active == 0 || n_consumed == 0 {
                continue;
            }
            // Every Active arm must be able to take the end, and the
            // source must be bound on every path to that arm's end:
            // its single def block dominates the arm, or it is a
            // header-bound param. Otherwise the end would free an
            // unbound register on this arm (103: bb0 is an Active arm
            // for _3, whose def sits in bb1).
            let mut insertable = true;
            for (&p, st) in &states {
                if !matches!(st, St::Active) {
                    continue;
                }
                let bound_here = match defs.get(s) {
                    Some(v) => v.len() == 1 && doms[p].contains(&v[0]),
                    None => uses.contains_key(s) && is_local_name(s),
                };
                if !bound_here {
                    insertable = false;
                    break;
                }
                match &blocks[p].terminator {
                    Term::Goto { .. } => {}
                    Term::Call { target: Some(_), .. } => {}
                    Term::Assert { .. } => {
                        if term_uses(&blocks[p].terminator, s) {
                            insertable = false;
                            break;
                        }
                    }
                    _ => {
                        insertable = false;
                        break;
                    }
                }
            }
            if !insertable {
                continue;
            }
            for (&p, st) in &states {
                if !matches!(st, St::Active) {
                    continue;
                }
                let is_call = matches!(&blocks[p].terminator, Term::Call { .. });
                let dummy = format!("_phi_{}_{}", blocks[p].id, s);
                push_move_free(p, s, dummy, is_call, &mut block_ends, &mut call_ends);
                // Later joins must see this consume as an event, or a
                // downstream join could re-classify the source Active
                // and free it a second time.
                let v = events.entry(s.clone()).or_default();
                if !v.contains(&p) {
                    v.push(p);
                }
            }
        }
    }
    // Deterministic order inside each block: borrower ends (Free)
    // first, sorted; then move-frees, with a borrower source before
    // its own source (a move-free of a source must not precede the end
    // of its borrower), ties by dummy name. Dedup exact duplicates
    // first; the sort above already orders each class.
    for v in block_ends.values_mut().chain(call_ends.values_mut()) {
        v.sort();
        v.dedup();
        let mut frees: Vec<JoinEnd> = vec![];
        let mut moves: Vec<JoinEnd> = vec![];
        for e in v.drain(..) {
            match e {
                JoinEnd::Free(_) => frees.push(e),
                JoinEnd::MoveFree { .. } => moves.push(e),
            }
        }
        // Kahn-style: an entry may go next only when no remaining entry
        // borrows its source (borrowers first). Borrow edges are
        // (borrower, source, _) triples.
        let mut ordered: Vec<JoinEnd> = vec![];
        while !moves.is_empty() {
            let mut picked: Option<usize> = None;
            for (i, e) in moves.iter().enumerate() {
                let src = e.reg();
                let blocked = moves
                    .iter()
                    .any(|o| o.reg() != src && edges.iter().any(|(b, s2, _)| b == o.reg() && s2 == src));
                if !blocked {
                    picked = Some(i);
                    break;
                }
            }
            match picked {
                Some(i) => ordered.push(moves.remove(i)),
                // Cycle (impossible in versioned MIR): keep dummy order
                // for the remainder rather than stalling.
                None => ordered.append(&mut moves),
            }
        }
        v.append(&mut frees);
        v.append(&mut ordered);
    }
    (block_ends, call_ends, subsumed_loop)
}

#[cfg(test)]
mod borrow_end_tests {
    use super::*;
    use crate::mir::Function;

    fn blk(id: &str, stmts: Vec<Stmt>, term: Term) -> Block {
        Block { id: id.to_string(), statements: stmts, terminator: term }
    }

    fn rfm(dest: &str, place: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: None,
            rvalue: Rvalue::Ref { place: place.to_string(), mut_: true, via: None, zst: false , off: 0, off2: 0, deref: false, deref_first: false, proj_loud: false , pty: None , deref_last: false },
        }
    }

    fn rf(dest: &str, place: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: None,
            rvalue: Rvalue::Ref { place: place.to_string(), mut_: false, via: None, zst: false , off: 0, off2: 0, deref: false, deref_first: false, proj_loud: false , pty: None , deref_last: false },
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
                args: vec![Operand::Move { place: "_2".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }],
                dest: Some("_3".to_string()), target: Some("bb1".to_string()),
                sig: None,
            }),
            blk("bb1", vec![], Term::Drop { place: "_1".to_string(), target: "bb2".to_string() }),
            blk("bb2", vec![], Term::Return { ret: None }),
        ]
    }

    fn store_blocks(use_after: bool) -> Vec<Block> {
        // bb0 (02 shape): _2 = &_1 (shared), _3 = copy _2 (last use),
        // _4 = &mut _1, store (*_4) = const; bb1 returns.
        let mut stmts = vec![
            rf("_2", "_1"),
            Stmt::Assign {
                dest: "_3".to_string(),
                dest_place: None,
                rvalue: Rvalue::Use { op: Operand::Copy { place: "_2".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
            },
            rfm("_4", "_1"),
            Stmt::Assign {
                dest: "_4".to_string(),
                dest_place: Some("(*_4)".to_string()),
                rvalue: Rvalue::Use { op: Operand::Move { place: "_9".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
            },
        ];
        if use_after {
            stmts.push(Stmt::Assign {
                dest: "_5".to_string(),
                dest_place: None,
                rvalue: Rvalue::Use { op: Operand::Copy { place: "_2".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
            });
        }
        vec![
            blk("bb0", stmts, Term::Goto { target: "bb1".to_string() }),
            blk("bb1", vec![], Term::Return { ret: None }),
        ]
    }

    #[test]
    fn store_conflict_ends() {
        let blocks = store_blocks(false);
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert_eq!(
            plan.store_ends.get(&(0, 3)),
            Some(&(vec!["_2".to_string()], vec!["_4".to_string()]))
        );
    }

    #[test]
    fn store_conflict_ends_used_after_veto() {
        // _2 reused after the store: no before-end (the store's own
        // borrow still ends after it).
        let blocks = store_blocks(true);
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert_eq!(
            plan.store_ends.get(&(0, 3)),
            Some(&(vec![], vec!["_4".to_string()]))
        );
    }

    #[test]
    fn linear_borrow_ended() {
        let blocks = lin();
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
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
            args: vec![Operand::Copy { place: "_2".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }],
            dest: None, target: None, sig: None,
        });
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert!(!plan.ends.contains_key(&1));
        assert!(plan.loud.contains(&1));
    }

    #[test]
    fn multidef_vetoes() {
        // Borrower redefined later: single-def gate refuses.
        let mut blocks = lin();
        blocks[2] = blk("bb2", vec![rf("_2", "_1")], Term::Return { ret: None });
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert!(!plan.ends.contains_key(&1));
        assert!(plan.loud.contains(&1));
    }

    #[test]
    fn unreachable_drop_stays_loud() {
        // Cleanup duplicate unreachable from entry: no `!b`, no `!p` (phase 2).
        let mut blocks = lin();
        blocks.push(blk("bb9", vec![], Term::Drop { place: "_1".to_string(), target: "bb2".to_string() }));
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
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
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
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
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert!(plan.ends.is_empty());
        assert!(plan.loud.is_empty());
    }

    #[test]
    fn zst_borrow_ignored() {
        // ZST borrows emit `= 0` (no `&`, no lock): no plan, no loud.
        let blocks = vec![
            blk("bb0", vec![Stmt::Assign {
                dest: "_2".to_string(), dest_place: None,
                rvalue: Rvalue::Ref { place: "_1".to_string(), mut_: false, via: None, zst: true , off: 0, off2: 0, deref: false, deref_first: false, proj_loud: false , pty: None , deref_last: false },
            }], Term::Goto { target: "bb1".to_string() }),
            blk("bb1", vec![], Term::Drop { place: "_1".to_string(), target: "bb2".to_string() }),
            blk("bb2", vec![], Term::Return { ret: None }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert!(plan.ends.is_empty());
        assert!(plan.loud.is_empty());
    }

    fn use_copy(dest: &str, place: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(), dest_place: None,
            rvalue: Rvalue::Use { op: Operand::Copy { place: place.to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
        }
    }

    fn use_move(dest: &str, place: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(), dest_place: None,
            rvalue: Rvalue::Use { op: Operand::Move { place: place.to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
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
            discr: Box::new(Operand::Move { place: discr.to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
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
                        left: Box::new(Operand::Move { place: "_6".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
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
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
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
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
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
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
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
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
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
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
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
                args: args.into_iter().map(|a| Operand::Copy { place: a.to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }).collect(),
                sig: None,
            },
        }
    }

    fn ct(func: &str, args: Vec<&str>, dest: Option<&str>, target: Option<&str>) -> Term {
        Term::Call {
            func: func.to_string(),
            func_raw: None,
            args: args.into_iter().map(|a| Operand::Copy { place: a.to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }).collect(),
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
                discr: Box::new(Operand::Copy { place: "_9".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
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
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        // _12 is taken over by 5a (its uses are all inside its def
        // block bb3, and bb3 dominates the back-edge block bb9), so its
        // loop end is pruned; _11 stays a loop end (phase 1 already
        // ends it at the exit Drop, so 5a vetoes it).
        assert_eq!(plan.loop_ends.get(&9), Some(&vec!["_11".to_string()]));
        assert_eq!(plan.block_ends.get(&3), Some(&vec![JoinEnd::Free("_12".to_string())]));
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
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        // _11's loop end is vetoed (live-out across iterations); _12's
        // loop end is pruned by the 5a takeover (see poll_shape).
        assert_eq!(plan.loop_ends.get(&9), None);
        assert_eq!(plan.block_ends.get(&3), Some(&vec![JoinEnd::Free("_12".to_string())]));
        assert_eq!(plan.ends.get(&5), Some(&vec!["_11".to_string()]));
    }

    #[test]
    fn loop_backedge_dropped_borrower_vetoes() {
        let mut blocks = poll_loop();
        blocks[6] = blk("bb6", vec![], Term::Drop { place: "_11".to_string(), target: "bb9".to_string() });
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        // _11's loop end is vetoed (MIR-dropped); _12's loop end is
        // pruned by the 5a takeover (see poll_shape).
        assert_eq!(plan.loop_ends.get(&9), None);
        assert_eq!(plan.block_ends.get(&3), Some(&vec![JoinEnd::Free("_12".to_string())]));
        assert!(plan.loud.contains(&5), "Drop _4 is loud once _11 is MIR-dropped");
    }

    #[test]
    fn loop_backedge_non_goto_skipped() {
        let mut blocks = poll_loop();
        blocks[9] = blk("bb9", vec![], ct("yield", vec![], None, Some("bb2")));
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert!(plan.loop_ends.is_empty(), "loop_ends: {:?}", plan.loop_ends);
    }

    #[test]
    fn loop_backedge_self_loop_skipped() {
        let blocks = vec![
            blk("bb0", vec![rf("_1", "_2")], Term::Goto { target: "bb0".into() }),
            blk("bb1", vec![], Term::Goto { target: "bb0".into() }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
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
    fn five_a_call_block_end() {
        // 108-shape: _5 = &_1 in bb0, consumed by bb0's own call; the
        // join after bb0 must see _1 unlocked, but phase 3 skips
        // Return joins. 5a ends _5 in bb0's call_ends (after the call,
        // before the jmp).
        let blocks = vec![
            blk("bb0", vec![rf("_5", "_1")], Term::Call {
                func: "next".to_string(), func_raw: None,
                args: vec![Operand::Copy { place: "_5".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }],
                dest: Some("_6".to_string()), target: Some("bb1".to_string()),
                sig: None,
            }),
            blk("bb1", vec![], Term::Return { ret: None }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert_eq!(plan.call_ends.get(&0), Some(&vec![JoinEnd::Free("_5".to_string())]));
    }

    #[test]
    fn five_b_goto_dummy_pair() {
        // 03-shape: _1 (param) moved on the bb1 arm, untouched on the
        // bb2 arm; both join at bb3 where _1 is dead. The Active arm
        // gets a move-into-dummy pair, not a bare free (a bare free
        // does not normalize a block-defined local at a join; the pair
        // works for params and locals alike).
        let blocks = vec![
            blk("bb0", vec![], switch("_c", "bb1", "bb2")),
            blk("bb1", vec![use_move("_4", "_1")], Term::Goto { target: "bb3".to_string() }),
            blk("bb2", vec![], Term::Goto { target: "bb3".to_string() }),
            blk("bb3", vec![], Term::Return { ret: None }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert_eq!(
            plan.block_ends.get(&2),
            Some(&vec![JoinEnd::MoveFree { dummy: "_phi_bb2__1".to_string(), src: "_1".to_string() }])
        );
        assert!(!plan.block_ends.contains_key(&1), "consumed arm takes no end");
    }

    #[test]
    fn five_b_unbound_arm_vetoed() {
        // 103-shape: _3 is defined in the join block bb1 itself, moved
        // on the back-edge arm bb9, untouched on the entry arm bb0. The
        // entry arm must NOT take an end: _3 is unbound on that path
        // (freeing it there trapped UnknownRegister).
        let blocks = vec![
            blk("bb0", vec![], Term::Goto { target: "bb1".to_string() }),
            blk("bb1", vec![const_bool("_3", "Val(Scalar(0x00), bool)")], Term::Goto { target: "bb2".to_string() }),
            blk("bb2", vec![], switch("_c", "bb9", "bb3")),
            blk("bb9", vec![use_move("_4", "_3")], Term::Goto { target: "bb1".to_string() }),
            blk("bb3", vec![], Term::Return { ret: None }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        let has_3 = plan.block_ends.values().chain(plan.call_ends.values()).flatten().any(|e| e.reg() == "_3");
        assert!(!has_3, "unbound arm must not take an end: {:?}", plan.block_ends);
    }

    #[test]
    fn five_b_spilled_source_no_fire() {
        // 03 with spill: _1's plain-assign Copy uses reload from a
        // slot, so they are not consuming events; both arms read
        // Active and no end fires (the old code counted reloads as
        // consumes and freed the source on the wrong arm).
        let blocks = vec![
            blk("bb0", vec![], switch("_c", "bb1", "bb2")),
            blk("bb1", vec![use_copy("_4", "_1")], Term::Goto { target: "bb3".to_string() }),
            blk("bb2", vec![], Term::Goto { target: "bb3".to_string() }),
            blk("bb3", vec![], Term::Return { ret: None }),
        ];
        let spilled: BTreeSet<String> = BTreeSet::from(["_1".to_string()]);
        let plan = plan_drops(&blocks, &doms(&blocks), &spilled);
        let has_1 = plan.block_ends.values().chain(plan.call_ends.values()).flatten().any(|e| e.reg() == "_1");
        assert!(!has_1, "spilled source takes no end: {:?}", plan.block_ends);
    }

    #[test]
    fn five_a_loop_takeover() {
        // 23-shape: _13 = &_11 in the loop head bb10, consumed by
        // bb10's own call; phase 4 already ended _13 at the back-edge
        // block bb11 (dominated by bb10). 5a takes over at the def
        // block — covering the loop-exit path phase 4 misses — and the
        // loop end is pruned (keeping both would double-free on the
        // body path).
        let blocks = vec![
            blk("bb10", vec![rf("_13", "_11")], Term::Call {
                func: "next".to_string(), func_raw: None,
                args: vec![Operand::Copy { place: "_13".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }],
                dest: Some("_12".to_string()), target: Some("bb11".to_string()),
                sig: None,
            }),
            blk("bb11", vec![], Term::Goto { target: "bb10".to_string() }),
        ];
        let succ = vec![vec![1], vec![0]];
        let edges = vec![("_13".to_string(), "_11".to_string(), 0usize)];
        let mut defs: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        defs.insert("_13".to_string(), vec![0]);
        let mut uses: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
        uses.insert("_13".to_string(), BTreeSet::from([0]));
        uses.insert("_11".to_string(), BTreeSet::from([0]));
        let entry_reach: HashSet<usize> = HashSet::from([0, 1]);
        let mut loop_ends: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        loop_ends.insert(1, vec!["_13".to_string()]);
        let (block_ends, call_ends, subsumed) = plan_join_ends(
            &blocks, &succ, &edges, &defs, &uses, &BTreeMap::new(), &BTreeSet::new(),
            &entry_reach, &doms(&blocks), &BTreeMap::new(), &BTreeMap::new(), &loop_ends,
            &BTreeSet::new(),
        );
        assert_eq!(call_ends.get(&0), Some(&vec![JoinEnd::Free("_13".to_string())]));
        assert!(subsumed.contains("_13"), "loop end must be pruned: {:?}", subsumed);
        assert!(block_ends.is_empty());
    }

    #[test]
    fn five_a_statement_moved_borrower_vetoed() {
        // 120-shape: _3 = &_1 in bb0, then moved into _2 by a RawPtr
        // statement in the same block. 5a must NOT end _3 after the
        // call: _3 is already consumed, and freeing it traps
        // (UnknownRegister on the consumed register).
        let rawptr = Stmt::Assign {
            dest: "_2".to_string(), dest_place: None,
            rvalue: Rvalue::RawPtr { place: "_3".to_string(), mut_: true , off: 0, off2: 0, deref: false, deref_first: false, proj_loud: false , pty: None , deref_last: false },
        };
        let blocks = vec![
            blk("bb0", vec![rf("_3", "_1"), rawptr, use_move("_5", "_2")], Term::Call {
                func: "read".to_string(), func_raw: None,
                args: vec![Operand::Move { place: "_5".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }],
                dest: Some("_4".to_string()), target: Some("bb1".to_string()),
                sig: None,
            }),
            blk("bb1", vec![], Term::Return { ret: None }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        let has_3 = plan.block_ends.values().chain(plan.call_ends.values()).flatten().any(|e| e.reg() == "_3");
        assert!(!has_3, "moved borrower must not take a 5a end: {:?}", plan.call_ends);
    }

    #[test]
    fn loop_backedge_def_must_dominate_backedge() {
        // 23-shape: _13 = &_11 is defined in the head bb2, but the
        // back-edge pred bb4 is reachable from the entry (bb0) without
        // passing bb2, so ending _13 in bb4 would free an unbound reg
        // on the first iteration (UnknownRegister). No loop end.
        let blocks = vec![
            blk("bb0", vec![], Term::SwitchInt {
                discr: Box::new(Operand::Copy { place: "_c".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
                targets: vec![("0".to_string(), "bb2".to_string())],
                otherwise: "bb4".to_string(),
            }),
            blk("bb2", vec![rf("_13", "_11"), ca("_12", "next", vec!["_13"])],
                Term::Goto { target: "bb3".into() }),
            blk("bb3", vec![], Term::SwitchInt {
                discr: Box::new(Operand::Copy { place: "_12".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
                targets: vec![("0".to_string(), "bb4".to_string())],
                otherwise: "bb5".to_string(),
            }),
            blk("bb4", vec![], Term::Goto { target: "bb2".into() }),
            blk("bb5", vec![], Term::Return { ret: None }),
        ];
        let plan = plan_drops(&blocks, &doms(&blocks), &BTreeSet::new());
        assert!(plan.loop_ends.is_empty(), "loop_ends: {:?}", plan.loop_ends);
    }
}
