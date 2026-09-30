//! mir2sa::version — SSA-style renaming for multi-def locals.
//!
//! SA registers are function-global single-assignment: a second textual
//! definition traps RegisterRedefinition, and a second move of one reg traps
//! UseAfterMove. MIR locals, however, are routinely redefined (mut params,
//! loop-carried variables, branch joins). This pass renames every definition
//! past the first: `_N` -> `_N_vK` (K = def occurrence in RPO order), and
//! rewrites each use to its reaching version:
//! - exactly one reaching version -> rename to it;
//! - none (params, single-def chains) -> keep the original name;
//! - two or more (join ambiguity, loop-carried merges) -> Conflict sentinel.
//!
//! Conflict sentinels (`Operand::Conflict`) flow into the existing loud
//! machinery via `const_needs_loud` (same predicate lower and coverage
//! share), so joins stay honest without phi nodes. Single-def locals are
//! never touched (lock-file stability for the common case).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::mir::{Block, Function, Operand, Rvalue, Stmt, Term};
use crate::order::rpo_order;

/// One definition site: (block index, statement index or None for terminator).
type DefSite = (usize, Option<usize>);

/// Collect def sites per local (Assign dests + Call dests + asm outs).
fn collect_defs(blocks: &[Block]) -> BTreeMap<String, Vec<DefSite>> {
    let mut defs: BTreeMap<String, Vec<DefSite>> = BTreeMap::new();
    for (bi, b) in blocks.iter().enumerate() {
        for (si, st) in b.statements.iter().enumerate() {
            if let Stmt::Assign { dest, .. } = st {
                defs.entry(dest.clone()).or_default().push((bi, Some(si)));
            }
        }
        match &b.terminator {
            Term::Call { dest: Some(d), .. } => {
                defs.entry(d.clone()).or_default().push((bi, None));
            }
            Term::InlineAsm { outs, .. } => {
                for o in outs {
                    defs.entry(o.clone()).or_default().push((bi, None));
                }
            }
            _ => {}
        }
    }
    defs
}

/// Reaching versions at block entries: IN[B][local] = set of versions that
/// can flow into B (fixpoint over predecessors; OUT[B] keeps only the latest
/// def per local since later defs kill earlier ones within B).
/// Keyed by version number K (into `_N_vK`).
fn reaching_in(
    blocks: &[Block],
    numbered: &BTreeMap<String, Vec<((usize, Option<usize>), usize)>>,
    pred: &[Vec<usize>],
    order: &[usize],
) -> Vec<BTreeMap<String, BTreeSet<usize>>> {
    let n = blocks.len();
    // Latest version defined per (block, local), incl. terminator dests.
    let mut out_of: Vec<BTreeMap<String, usize>> = vec![BTreeMap::new(); n];
    for (bi, b) in blocks.iter().enumerate() {
        // Walk the block's own statements in order, terminator dest last.
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        for (si, st) in b.statements.iter().enumerate() {
            if let Stmt::Assign { dest, .. } = st {
                if let Some(v) = numbered.get(dest) {
                    if let Some((_, k)) = v.iter().find(|((db, dp), _)| *db == bi && *dp == Some(si)) {
                        seen.insert(dest.clone(), *k);
                    }
                }
            }
        }
        if let Term::Call { dest: Some(d), .. } = &b.terminator {
            if let Some(v) = numbered.get(d) {
                if let Some((_, k)) = v.iter().find(|((db, dp), _)| *db == bi && dp.is_none()) {
                    seen.insert(d.clone(), *k);
                }
            }
        }
        if let Term::InlineAsm { outs, .. } = &b.terminator {
            for o in outs {
                if let Some(v) = numbered.get(o) {
                    if let Some((_, k)) = v.iter().find(|((db, dp), _)| *db == bi && dp.is_none()) {
                        seen.insert(o.clone(), *k);
                    }
                }
            }
        }
        out_of[bi] = seen;
    }
    let mut inn: Vec<BTreeMap<String, BTreeSet<usize>>> = vec![BTreeMap::new(); n];
    let mut changed = true;
    while changed {
        changed = false;
        for &bi in order {
            let mut merged: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
            for &p in &pred[bi] {
                for (l, k) in &out_of[p] {
                    merged.entry(l.clone()).or_default().insert(*k);
                }
                // Hmm: predecessors' IN also flows when pred defines nothing?
                // No — OUT[p] already = latest defs in p... but what about
                // versions flowing THROUGH p (defined before p, used after)?
                // Standard reaching-definitions: OUT[p] = (IN[p] - killed) + gen.
                // My out_of only has GEN. FIX below: merge IN too.
                for (l, ks) in &inn[p] {
                    // killed if p redefines l (out_of has it)
                    if !out_of[p].contains_key(l) {
                        merged.entry(l.clone()).or_default().extend(ks.iter().cloned());
                    }
                }
            }
            if merged != inn[bi] {
                inn[bi] = merged;
                changed = true;
            }
        }
    }
    inn
}

/// Resolve one use to a version number (into `_N_vK`), [`None`] for keep-name,
/// or conflict (caller emits the sentinel).
/// Rule (reaching definitions): same-block earlier defs win by program order
/// (unambiguous); otherwise the IN set decides — exactly one reaching version
/// renames, two or more conflict, none keeps the name.
fn resolve(
    local: &str,
    user_block: usize,
    user_pos: Option<usize>,
    numbered: &BTreeMap<String, Vec<((usize, Option<usize>), usize)>>,
    inn: &[BTreeMap<String, BTreeSet<usize>>],
) -> Result<Option<usize>, ()> {
    let defs = match numbered.get(local) {
        Some(d) => d,
        None => return Ok(None),
    };
    let mut best: Option<(i64, usize)> = None;
    for ((db, dp), k) in defs {
        if *db != user_block {
            continue;
        }
        let key = match (*dp, user_pos) {
            // Terminator uses see all statement defs of the block.
            (Some(si), None) => si as i64,
            (Some(d), Some(u)) if d < u => d as i64,
            _ => continue,
        };
        if best.map(|(pk, _)| key > pk).unwrap_or(true) {
            best = Some((key, *k));
        }
    }
    if let Some((_, k)) = best {
        return Ok(Some(k));
    }
    match inn[user_block].get(local) {
        // Multi-def but unreached here: the base name is gone (defs were
        // renamed), so keeping it would dangle -> loud, not silent.
        None => Err(()),
        Some(ks) if ks.len() == 1 => Ok(Some(*ks.iter().next().unwrap())),
        Some(_) => Err(()),
    }
}

fn versioned_name(local: &str, k: usize) -> String {
    format!("{}_v{}", local, k)
}

/// Rewrite one operand use site.
fn rewrite_operand(
    op: &mut Operand,
    user_block: usize,
    user_pos: Option<usize>,
    numbered: &BTreeMap<String, Vec<((usize, Option<usize>), usize)>>,
    inn: &[BTreeMap<String, BTreeSet<usize>>],
) {
    let local = match op {
        Operand::Move { place } | Operand::Copy { place } => place.clone(),
        _ => return,
    };
    match resolve(&local, user_block, user_pos, numbered, inn) {
        Ok(Some(k)) => {
            let new = versioned_name(&local, k);
            match op {
                Operand::Move { place } | Operand::Copy { place } => *place = new,
                _ => {}
            }
        }
        Err(()) => {
            *op = Operand::Conflict { place: local };
        }
        Ok(None) => {}
    }
}

/// Rewrite one place use site (Ref/RawPtr/Discriminant/SetDisc/Drop).
/// Returns true on version conflict (caller replaces the enclosing
/// rvalue/statement/terminator with Unsupported — places have no sentinel
/// shape of their own).
fn rewrite_place(
    place: &mut String,
    user_block: usize,
    numbered: &BTreeMap<String, Vec<((usize, Option<usize>), usize)>>,
    inn: &[BTreeMap<String, BTreeSet<usize>>],
) -> bool {
    if numbered.contains_key(place) {
        match resolve(place, user_block, None, numbered, inn) {
            Ok(Some(k)) => {
                *place = versioned_name(place, k);
            }
            Err(()) => {
                return true;
            }
            Ok(None) => {}
        }
    }
    false
}

/// Version one function: rename multi-def dests, rewrite uses, mark join
/// conflicts. Single-def functions return unchanged (fast path).
pub fn version_function(f: &Function) -> Function {
    let defs = collect_defs(&f.blocks);
    let multi: BTreeSet<String> =
        defs.iter().filter(|(_, v)| v.len() > 1).map(|(k, _)| k.clone()).collect();
    if multi.is_empty() {
        return Function {
            name: f.name.clone(),
            locals: f.locals.clone(),
            params: f.params.clone(),
            ret: f.ret.clone(),
            sig_ok: f.sig_ok,
            blocks: f.blocks.clone(),
        };
    }
    let order = rpo_order(&f.blocks);
    // Predecessors for the reaching-definitions fixpoint.
    let n = f.blocks.len();
    let index: HashMap<&str, usize> =
        f.blocks.iter().enumerate().map(|(i, b)| (b.id.as_str(), i)).collect();
    let mut pred: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, b) in f.blocks.iter().enumerate() {
        let ts: Vec<&str> = match &b.terminator {
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
        };
        for s in ts {
            if let Some(&j) = index.get(s) {
                pred[j].push(i);
            }
        }
    }
    // Number defs in RPO order: (block, pos) -> version K.
    let mut numbered: BTreeMap<String, Vec<((usize, Option<usize>), usize)>> = BTreeMap::new();
    {
        // Walk statements in RPO order for deterministic numbering.
        let mut order_pos: BTreeMap<(usize, Option<usize>), usize> = BTreeMap::new();
        for (rank, bi) in order.iter().enumerate() {
            let b = &f.blocks[*bi];
            for (si, _) in b.statements.iter().enumerate() {
                order_pos.insert((*bi, Some(si)), rank * 1_000_000 + si);
            }
            order_pos.insert((*bi, None), rank * 1_000_000 + 999_999);
        }
        for local in &multi {
            let mut sites = defs[local].clone();
            sites.sort_by_key(|s| order_pos.get(s).copied().unwrap_or(usize::MAX));
            numbered.insert(
                local.clone(),
                sites.into_iter().enumerate().map(|(k, s)| (s, k)).collect(),
            );
        }
    }
    let version_of = |local: &str, bi: usize, pos: Option<usize>| -> Option<usize> {
        numbered.get(local).and_then(|v| {
            v.iter().find(|((b, p), _)| *b == bi && *p == pos).map(|(_, k)| *k)
        })
    };
    let inn = reaching_in(&f.blocks, &numbered, &pred, &order);
    let mut out = f.clone();
    for (bi, b) in out.blocks.iter_mut().enumerate() {
        for (si, st) in b.statements.iter_mut().enumerate() {
            match st {
                Stmt::Assign { dest, rvalue, .. } => {
                    // Rewrite operand uses first (they read pre-assign versions).
                    let pos = Some(si);
                    match rvalue {
                        Rvalue::Use { op } => {
                            rewrite_operand(op, bi, pos, &numbered, &inn)
                        }
                        Rvalue::Call { args, .. } => {
                            for a in args {
                                rewrite_operand(a, bi, pos, &numbered, &inn);
                            }
                        }
                        Rvalue::BinOp { left, right, .. } => {
                            rewrite_operand(left, bi, pos, &numbered, &inn);
                            rewrite_operand(right, bi, pos, &numbered, &inn);
                        }
                        Rvalue::UnOp { operand, .. } => {
                            rewrite_operand(operand, bi, pos, &numbered, &inn)
                        }
                        Rvalue::Cast { op, .. } => {
                            rewrite_operand(op, bi, pos, &numbered, &inn)
                        }
                        Rvalue::Aggregate { elems, .. } => {
                            for e in elems {
                                rewrite_operand(e, bi, pos, &numbered, &inn);
                            }
                        }
                        Rvalue::Repeat { op, .. } => {
                            rewrite_operand(op, bi, pos, &numbered, &inn)
                        }
                        Rvalue::ThreadLocal { .. } => {}
                        Rvalue::Ref { place, .. }
                        | Rvalue::Discriminant { place }
                        | Rvalue::RawPtr { place, .. } => {
                            // Places have no sentinel shape: on conflict the
                            // whole rvalue goes loud (counted downstream).
                            if rewrite_place(place, bi, &numbered, &inn) {
                                *rvalue = Rvalue::Unsupported {
                                    text: "version conflict at join (multiple reaching defs)".to_string(),
                                };
                            }
                        }
                        Rvalue::Unsupported { .. } => {}
                    }
                    // Then rename the def itself (multi-def only).
                    if multi.contains(dest) {
                        if let Some(k) = version_of(dest, bi, Some(si)) {
                            *dest = versioned_name(dest, k);
                        }
                    }
                }
                Stmt::SetDisc { place, .. } => {
                    if rewrite_place(place, bi, &numbered, &inn) {
                        *st = Stmt::UnsupportedStmt {
                            text: "version conflict at join (multiple reaching defs)".to_string(),
                        };
                    }
                }
                _ => {}
            }
        }
        // Terminator positions (after all statement defs of the block).
        match &mut b.terminator {
            Term::Call { args, dest, .. } => {
                for a in args {
                    rewrite_operand(a, bi, None, &numbered, &inn);
                }
                if let Some(d) = dest {
                    if multi.contains(d) {
                        if let Some(k) = version_of(d, bi, None) {
                            *d = versioned_name(d, k);
                        }
                    }
                }
            }
            Term::Drop { place, .. } => {
                // Cannot assign through the match borrow: flag then replace.
                if rewrite_place(place, bi, &numbered, &inn) {
                    place.clear();
                    place.push_str("__VERSION_CONFLICT__");
                }
            }
            Term::SwitchInt { discr, .. } => {
                rewrite_operand(discr, bi, None, &numbered, &inn)
            }
            Term::Assert { cond, .. } => {
                rewrite_operand(cond, bi, None, &numbered, &inn)
            }
            Term::InlineAsm { outs, ins, .. } => {
                for o in outs.iter_mut() {
                    // Outs are defs: version when multi-def.
                    if multi.contains(o) {
                        if let Some(k) = version_of(o, bi, None) {
                            *o = versioned_name(o, k);
                        }
                    }
                }
                for i in ins {
                    rewrite_operand(i, bi, None, &numbered, &inn);
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod version_tests {
    use super::*;
    use crate::mir::{Operand, Rvalue, Stmt, Term};

    fn use_stmt(dest: &str, place: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: Some(dest.to_string()),
            rvalue: Rvalue::Use { op: Operand::Copy { place: place.to_string() } },
        }
    }

    fn blk(id: &str, stmts: Vec<Stmt>, term: Term) -> Block {
        Block { id: id.to_string(), statements: stmts, terminator: term }
    }

    fn goto(t: &str) -> Term {
        Term::Goto { target: t.to_string() }
    }

    fn mkfn(blocks: Vec<Block>) -> Function {
        Function {
            name: "f".to_string(),
            locals: vec![],
            params: vec![],
            ret: None,
            sig_ok: true,
            blocks,
        }
    }

    #[test]
    fn single_def_passthrough() {
        let f = mkfn(vec![blk("bb0", vec![use_stmt("_1", "_2")], goto("bb1")),
                         blk("bb1", vec![], Term::Return)]);
        let out = version_function(&f);
        assert_eq!(out.blocks[0].statements.len(), 1);
    }

    #[test]
    fn chain_versions() {
        // bb0 defines _1 twice (v0, v1); bb1 dominated by bb0 uses _1 -> v1.
        let f = mkfn(vec![
            blk("bb0", vec![use_stmt("_1", "_9"), use_stmt("_1", "_8")], goto("bb1")),
            blk("bb1", vec![use_stmt("_2", "_1")], Term::Return),
        ]);
        let out = version_function(&f);
        // Both defs renamed; the use sees the deepest (v1).
        if let Stmt::Assign { dest: d0, .. } = &out.blocks[0].statements[0] {
            assert_eq!(d0, "_1_v0");
        } else {
            panic!("shape");
        }
        if let Stmt::Assign { dest: d1, .. } = &out.blocks[0].statements[1] {
            assert_eq!(d1, "_1_v1");
        } else {
            panic!("shape");
        }
        if let Stmt::Assign { rvalue: Rvalue::Use { op: Operand::Copy { place } }, .. } =
            &out.blocks[1].statements[0]
        {
            assert_eq!(place, "_1_v1");
        } else {
            panic!("shape");
        }
    }

    #[test]
    fn join_conflict_sentinel() {
        // Diamond: _1 defined in both branches; join use conflicts.
        let f = mkfn(vec![
            blk("bb0", vec![], Term::SwitchInt {
                discr: Box::new(Operand::Copy { place: "_9".to_string() }),
                targets: vec![("0".to_string(), "bb1".to_string())],
                otherwise: "bb2".to_string(),
            }),
            blk("bb1", vec![use_stmt("_1", "_8")], goto("bb3")),
            blk("bb2", vec![use_stmt("_1", "_7")], goto("bb3")),
            blk("bb3", vec![use_stmt("_2", "_1")], Term::Return),
        ]);
        let out = version_function(&f);
        if let Stmt::Assign { rvalue: Rvalue::Use { op }, .. } = &out.blocks[3].statements[0] {
            assert!(matches!(op, Operand::Conflict { .. }));
        } else {
            panic!("shape");
        }
    }

    #[test]
    fn stmt_plus_asm_out_reaches() {
        // 117 shape: stmt def + asm-out def in bb0, Ref use in bb1.
        // The use must see the terminator version (v1), not dangle.
        // The use must see the terminator version (v1), not dangle.
        let asm_term = Term::InlineAsm {
            text: "asm".to_string(),
            template: Some("/* nop */".to_string()),
            options: Some("".to_string()),
            modifiers: false,
            inout: true,
            outs: vec!["_1".to_string()],
            ins: vec![Operand::Copy { place: "_1".to_string() }],
            target: Some("bb1".to_string()),
        };
        let ref_stmt = Stmt::Assign {
            dest: "_5".to_string(),
            dest_place: Some("_5".to_string()),
            rvalue: Rvalue::Ref { place: "_1".to_string(), mut_: false, via: None, zst: false },
        };
        let f = mkfn(vec![
            blk("bb0", vec![use_stmt("_1", "_9")], asm_term),
            blk("bb1", vec![ref_stmt], Term::Return),
        ]);
        let out = version_function(&f);
        if let Stmt::Assign { rvalue: Rvalue::Ref { place, .. }, .. } = &out.blocks[1].statements[0] {
            assert_eq!(place, "_1_v1", "asm-out version must reach");
        } else {
            panic!("shape");
        }
    }
    #[test]
    fn loop_carried_conflicts() {
        // bb0 defines _2; a side path redefines it in bb3 and rejoins at
        // bb5 where it is borrowed. Both versions reach bb5 with neither
        // dominating -> the Ref keeps the base name (places have no
        // sentinel shape; documented approximation), but both defs must
        // be versioned.
        let mkref = || Stmt::Assign {
            dest: "_5".to_string(),
            dest_place: Some("_5".to_string()),
            rvalue: Rvalue::Ref { place: "_2".to_string(), mut_: false, via: None, zst: false },
        };
        let f = mkfn(vec![
            blk("bb0", vec![use_stmt("_2", "_9")], Term::SwitchInt {
                discr: Box::new(Operand::Copy { place: "_9".to_string() }),
                targets: vec![("0".to_string(), "bb5".to_string())],
                otherwise: "bb2".to_string(),
            }),
            blk("bb2", vec![], goto("bb3")),
            blk("bb3", vec![use_stmt("_2", "_6")], goto("bb4")),
            blk("bb4", vec![], goto("bb5")),
            blk("bb5", vec![mkref()], Term::Return),
        ]);
        let out = version_function(&f);
        if let Stmt::Assign { dest: d0, .. } = &out.blocks[0].statements[0] {
            assert_eq!(d0, "_2_v0");
        } else {
            panic!("shape");
        }
        if let Stmt::Assign { dest: d3, .. } = &out.blocks[2].statements[0] {
            assert_eq!(d3, "_2_v1");
        } else {
            panic!("shape");
        }
    }

}

