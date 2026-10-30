//! mir2sa::phi — join-version (phi) resolution through a per-local slot.
//!
//! Background (see version.rs): SA registers are single-assignment per
//! path, so a MIR local redefined on two paths reaching one join has no
//! single register at the join — version.rs marks those uses Conflict (or
//! `__PHI__:` places, below) and lower() went loud. This module resolves
//! the provable cases without guessing a version:
//!
//! For each conflicted base local `L` with a mappable scalar/pointer type,
//! lower() allocates one 8-byte slot `_L_spill` in the entry block, stores
//! every versioned definition of `L` into it immediately after the def,
//! and loads the slot into a fresh temp at each conflict use. On any
//! executed path the slot holds exactly the last definition on that path
//! (defs store in program order; the entry param store covers pre-def
//! param uses), so the load is exact, not a guessed version. Loops work
//! the same way: the back-edge def stores before the jump to the header,
//! so the header load sees the current iteration's value.
//!
//! Soundness gates (violations stay loud, never silently resolved):
//! - Type must map to an SA store type (integers, bool, usize/isize,
//!   char, thin/raw pointers, NonNull wrappers). ADT/float/128-bit and
//!   unknown types stay loud.
//! - The base local must not already own a spill slot (collision guard).
//! - `&mut` borrows of a conflict place stay loud: a load-temp borrow
//!   would point at the temp copy, and writes through it would never
//!   write back to the local or its slot.
//! - Shared borrows resolve only when the use block dominates every
//!   return block: the borrow temp is released by drop glue at exits,
//!   which is dominance-gated, so a non-dominating borrow temp could
//!   leak on some return path. Other temps are freed in-block by lower()
//!   (reads) or consumed by their use (moves/returns), so they need no
//!   dominance guard.
//! - Drops of a conflict place stay loud for now: freeing a slot-loaded
//!   temp and later freeing the versioned source register would release
//!   the same allocation twice. Return values resolve only for scalar
//!   types (no allocation to double-free); pointer returns stay loud.
//! - A load temp is freed after its use exactly when the use reads it
//!   (BinOp/UnOp/Cast-convert/Aggregate-multi/Repeat/call-arg/switch/
//!   assert/discriminant/setdisc). Plain-assign uses move the temp, and
//!   borrowed temps are left to drop glue, so those are never freed here
//!   (freeing a moved or borrowed register would trap).
//!
//! Projected conflicts (join-05): version.rs preserves a conflicted
//! field read's projection on the Conflict, and build_phi gives a
//! buffer-shaped base (every whole def a multi-elem Aggregate, Repeat,
//! or buf-typed Use) a `ptr` slot even when its MIR type is an ADT:
//! the SA value of such a local IS its buffer pointer. rewrite_operand
//! then loads the slot pointer and extracts the field in its load
//! lines, so the rewritten operand is a plain value temp and every
//! existing free rule (Use moves it, BinOp/call/switch free it) stays
//! exact. Aggregate-typed field copies (pty "buf", not a trailing
//! deref), untyped projections, driver-flagged projections, and
//! projected reads over non-ptr slots stay loud, never silently
//! resolved. Whole-aggregate conflicts keep today's loud path: only
//! field reads (a driver field type) qualify for a buffer slot.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::mir::{Block, Operand, Rvalue, Stmt, Term};
use crate::spill::{spill_slot, SpillMap};

/// Marker prefix version.rs writes into a conflict place (Ref /
/// Discriminant / RawPtr / SetDisc / Drop / Return) in place of the
/// versioned name: the kind, mutability and variant survive, only the
/// ambiguous place is marked. Lower resolves or goes loud from it.
pub const PHI_MARKER_PREFIX: &str = "__PHI__:";

/// Marker text for a conflict place over base local `local`.
pub fn phi_marker(local: &str) -> String {
    format!("{}{}", PHI_MARKER_PREFIX, local)
}

/// Base local behind a `__PHI__:` place, or None for ordinary places.
pub fn phi_place(place: &str) -> Option<&str> {
    place.strip_prefix(PHI_MARKER_PREFIX)
}

/// SA store type for a phi slot from a driver local type, or None when
/// the type is not a provable scalar/pointer (stays loud).
pub fn phi_slot_ty(ty: &str) -> Option<String> {
    let t = ty.trim();
    if t == "ptr" || t.starts_with('*') || t.starts_with('&') {
        return Some("ptr".to_string());
    }
    // NonNull<T> is repr(transparent) over one pointer (same precedent as
    // spill.rs's pointer-valued treatment of NonNull wrappers).
    if t.starts_with("std::ptr::NonNull") || t.starts_with("NonNull") {
        return Some("ptr".to_string());
    }
    match t {
        "bool" => Some("u8".to_string()),
        "usize" => Some("u64".to_string()),
        "isize" => Some("i64".to_string()),
        "char" => Some("u32".to_string()),
        "u8" | "i8" | "u16" | "i16" | "u32" | "i32" | "u64" | "i64" => {
            Some(t.to_string())
        }
        _ => None,
    }
}

/// Base local behind a legacy version-conflict Unsupported text
/// (`version conflict at join (multiple reaching defs of _N)`), if any.
fn conflict_text_local(text: &str) -> Option<String> {
    let rest = text.strip_prefix("version conflict at join (multiple reaching defs of ")?;
    let base = rest.strip_suffix(')')?;
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

fn insert_phi(
    phi: &mut BTreeMap<String, String>,
    local_types: &BTreeMap<String, String>,
    ret: Option<&str>,
    spill: &SpillMap,
    excluded: &BTreeSet<String>,
    base: &str,
) {
    if phi.contains_key(base) || spill.contains_key(base) || excluded.contains(base) {
        return;
    }
    let ty = local_types
        .get(base)
        .and_then(|t| phi_slot_ty(t))
        .or_else(|| {
            if base == "_0" {
                ret.and_then(phi_slot_ty)
            } else {
                None
            }
        });
    if let Some(ty) = ty {
        phi.insert(base.to_string(), ty);
    }
}

fn insert_phi_ty(
    phi: &mut BTreeMap<String, String>,
    spill: &SpillMap,
    excluded: &BTreeSet<String>,
    base: &str,
    ty: &str,
) {
    if phi.contains_key(base) || spill.contains_key(base) || excluded.contains(base) {
        return;
    }
    phi.insert(base.to_string(), ty.to_string());
}

fn scan_operand(
    phi: &mut BTreeMap<String, String>,
    local_types: &BTreeMap<String, String>,
    ret: Option<&str>,
    spill: &SpillMap,
    excluded: &BTreeSet<String>,
    buffer_bases: &BTreeSet<String>,
    op: &Operand,
) {
    if let Operand::Conflict { place, pty, .. } = op {
        insert_phi(phi, local_types, ret, spill, excluded, place);
        // Buffer field read (join-05): the MIR type is an ADT, so the
        // scalar/pointer rule above yields nothing, but the SA value
        // is a buffer pointer. Only field reads (a driver field type)
        // qualify: whole-aggregate conflicts keep today's loud path.
        if !phi.contains_key(place) && pty.is_some() && buffer_bases.contains(place) {
            insert_phi_ty(phi, spill, excluded, place, "ptr");
        }
    }
}

fn scan_rvalue(
    phi: &mut BTreeMap<String, String>,
    local_types: &BTreeMap<String, String>,
    ret: Option<&str>,
    spill: &SpillMap,
    excluded: &BTreeSet<String>,
    buffer_bases: &BTreeSet<String>,
    rv: &Rvalue,
) {
    match rv {
        Rvalue::Use { op } => scan_operand(phi, local_types, ret, spill, excluded, buffer_bases, op),
        Rvalue::BinOp { left, right, .. } => {
            scan_operand(phi, local_types, ret, spill, excluded, buffer_bases, left);
            scan_operand(phi, local_types, ret, spill, excluded, buffer_bases, right);
        }
        Rvalue::UnOp { operand, .. } | Rvalue::Cast { op: operand, .. } => {
            scan_operand(phi, local_types, ret, spill, excluded, buffer_bases, operand)
        }
        Rvalue::Aggregate { elems, .. } => {
            for e in elems {
                scan_operand(phi, local_types, ret, spill, excluded, buffer_bases, e);
            }
        }
        Rvalue::Repeat { op, .. } => scan_operand(phi, local_types, ret, spill, excluded, buffer_bases, op),
        Rvalue::Call { args, .. } => {
            for a in args {
                scan_operand(phi, local_types, ret, spill, excluded, buffer_bases, a);
            }
        }
        Rvalue::Ref { place, .. }
        | Rvalue::Discriminant { place }
        | Rvalue::RawPtr { place, .. } => {
            if let Some(base) = phi_place(place) {
                insert_phi(phi, local_types, ret, spill, excluded, base);
            }
        }
        Rvalue::Unsupported { text } => {
            if let Some(base) = conflict_text_local(text) {
                insert_phi(phi, local_types, ret, spill, excluded, &base);
            }
        }
        Rvalue::ThreadLocal { .. } => {}
    }
}

/// Bases whose SA value is a buffer pointer: every whole (non-place-store)
/// definition of the base is a buffer-shaped rvalue (multi-elem Aggregate,
/// Repeat, or a buf-typed Use) — the same shape lower() uses for its
/// buf_def set, but keyed by base local so join conflicts can find it.
/// "Whole" is judged on versioned blocks as dest_place equal to the dest
/// or to its unversioned base, since version.rs leaves dest_place at the
/// base name while renaming the dest.
/// A base with any non-buffer def (call result, single-elem aggregate,
/// scalar op) is excluded: its SA value is not provably a pointer.
fn buffer_bases(blocks: &[Block]) -> BTreeSet<String> {
    let mut all: BTreeMap<String, bool> = BTreeMap::new();
    for b in blocks {
        for st in &b.statements {
            if let Stmt::Assign { dest, dest_place, rvalue } = st {
                // Whole-def test on VERSIONED blocks: version.rs renames
                // the dest (_4 -> _4_v0) but leaves dest_place at the
                // base name ("_4"), so raw inequality would misclassify
                // every versioned whole def as a place store. A place
                // store's dest_place is place text ("(_4_v0.1: u64)"),
                // never the bare dest or its base.
                let base0 = crate::asm::strip_version_suffix(dest);
                if dest_place.as_deref().is_some_and(|dp| {
                    let dp = dp.trim();
                    dp != dest.trim() && dp != base0
                }) {
                    continue;
                }
                let is_buf = match rvalue {
                    Rvalue::Aggregate { elems, .. } => elems.len() >= 2,
                    Rvalue::Repeat { .. } => true,
                    Rvalue::Use { op } => matches!(op,
                        Operand::Move { pty, .. } | Operand::Copy { pty, .. }
                        if pty.as_deref() == Some("buf")),
                    _ => false,
                };
                let base = crate::asm::strip_version_suffix(dest).to_string();
                let e = all.entry(base).or_insert(true);
                *e = *e && is_buf;
            }
        }
        match &b.terminator {
            Term::Call { dest: Some(d), .. } => {
                let base = crate::asm::strip_version_suffix(d).to_string();
                all.insert(base, false);
            }
            Term::InlineAsm { outs, .. } => {
                for o in outs {
                    let base = crate::asm::strip_version_suffix(o).to_string();
                    all.insert(base, false);
                }
            }
            _ => {}
        }
    }
    all.into_iter().filter(|(_, ok)| *ok).map(|(base, _)| base).collect()
}

/// Phi map for one versioned function: base local -> SA store type.
/// Only reachable blocks are scanned, and bases defined in unreachable
/// blocks are excluded (see the exclusion note in the body). Drops are
/// deliberately NOT scanned: drop conflicts stay loud (see the module
/// docs), so they need no slot.
pub fn build_phi(
    blocks: &[Block],
    local_types: &BTreeMap<String, String>,
    ret: Option<&str>,
    spill: &SpillMap,
    reachable: &HashSet<usize>,
) -> BTreeMap<String, String> {
    // Bases with any definition in an unreachable (cleanup) block are
    // excluded outright: their slot stores would be skipped (unreachable
    // blocks never resolve), so a reachable load could read a stale slot
    // on an unwind path — unprovable, so it stays loud.
    let mut excluded: BTreeSet<String> = BTreeSet::new();
    for (bi, b) in blocks.iter().enumerate() {
        if reachable.contains(&bi) {
            continue;
        }
        for st in &b.statements {
            if let Stmt::Assign { dest, .. } = st {
                excluded.insert(crate::asm::strip_version_suffix(dest).to_string());
            }
        }
        match &b.terminator {
            Term::Call { dest: Some(d), .. } => {
                excluded.insert(crate::asm::strip_version_suffix(d).to_string());
            }
            Term::InlineAsm { outs, .. } => {
                for o in outs {
                    excluded.insert(crate::asm::strip_version_suffix(o).to_string());
                }
            }
            _ => {}
        }
    }
    let mut phi: BTreeMap<String, String> = BTreeMap::new();
    let buf_bases = buffer_bases(blocks);
    for (bi, b) in blocks.iter().enumerate() {
        // Unreachable (cleanup) blocks never resolve: their slot loads
        // trap in `sa check` once the slot was freed on a normal return
        // path, and their IN sets are empty in the normal CFG (the
        // conflict there is an unreached use, not a true join).
        if !reachable.contains(&bi) {
            continue;
        }
        for st in &b.statements {
            match st {
                Stmt::Assign { rvalue, .. } => {
                    scan_rvalue(&mut phi, local_types, ret, spill, &excluded, &buf_bases, rvalue)
                }
                Stmt::SetDisc { place, .. } => {
                    if let Some(base) = phi_place(place) {
                        insert_phi(&mut phi, local_types, ret, spill, &excluded, base);
                    }
                }
                Stmt::UnsupportedStmt { text } => {
                    if let Some(base) = conflict_text_local(text) {
                        insert_phi(&mut phi, local_types, ret, spill, &excluded, &base);
                    }
                }
                _ => {}
            }
        }
        match &b.terminator {
            Term::Call { args, .. } => {
                for a in args {
                    scan_operand(&mut phi, local_types, ret, spill, &excluded, &buf_bases, a);
                }
            }
            Term::SwitchInt { discr, .. } => {
                scan_operand(&mut phi, local_types, ret, spill, &excluded, &buf_bases, discr)
            }
            Term::Assert { cond, .. } => {
                scan_operand(&mut phi, local_types, ret, spill, &excluded, &buf_bases, cond)
            }
            Term::InlineAsm { ins, .. } => {
                for i in ins {
                    scan_operand(&mut phi, local_types, ret, spill, &excluded, &buf_bases, i);
                }
            }
            Term::Return { ret: Some(r) } => {
                if let Some(base) = phi_place(r) {
                    insert_phi(&mut phi, local_types, ret, spill, &excluded, base);
                }
            }
            _ => {}
        }
    }
    phi
}

/// True when a shared-borrow temp in block `bi` may resolve: the block
/// must dominate every return block (drop glue releases borrow temps
/// only at dominated exits). `doms` are order::dom_sets, `returns` the
/// indices of Return-terminated blocks.
pub fn phi_ref_allowed(doms: &[HashSet<usize>], returns: &[usize], bi: usize) -> bool {
    returns.iter().all(|&r| doms.get(r).is_some_and(|d| d.contains(&bi)))
}

/// One rewritten operand: the new operand, the load lines to emit
/// before its use (a projected field read needs two: the slot load and
/// the field load), and the temp register (if any). None when the
/// operand is an unresolvable phi item (caller keeps the loud path).
/// Non-phi operands pass through unchanged with no load.
pub fn rewrite_operand(
    op: &Operand,
    phi: &BTreeMap<String, String>,
    bid: &str,
    idx: &mut usize,
) -> Option<(Operand, Vec<String>, Option<String>)> {
    match op {
        Operand::Conflict { place, off, off2, deref, deref_first, deref_last, pty, proj_loud } => {
            let ty = phi.get(place)?;
            if *proj_loud {
                return None;
            }
            let is_field = pty.is_some() || *off != 0 || *deref;
            if !is_field {
                let temp = format!("_phi_{}_{}", bid, *idx);
                *idx += 1;
                let load = format!("{} = load {}+0 as {}", temp, spill_slot(place), ty);
                return Some((plain_copy(&temp), vec![load], Some(temp)));
            }
            // Projected field read: only a ptr slot can be field-extracted
            // (a scalar slot holds a value, not a buffer). Untyped
            // projections and aggregate-typed field copies stay loud:
            // the former cannot type its load, the latter would share
            // the buffer under later place stores (aggcopy rule).
            if ty != "ptr" || pty.is_none() {
                return None;
            }
            if pty.as_deref() == Some("buf") && !(*deref && *deref_last) {
                return None;
            }
            if pty.as_deref() == Some("zst") {
                let temp = format!("_phi_{}_{}", bid, *idx);
                *idx += 1;
                return Some((plain_copy(&temp), vec![format!("{} = 0", temp)], Some(temp)));
            }
            let ptr_temp = format!("_phi_{}_{}", bid, *idx);
            *idx += 1;
            let field_temp = format!("_phi_{}_{}", bid, *idx);
            *idx += 1;
            let proj_op = Operand::Copy {
                place: ptr_temp.clone(),
                off: *off,
                off2: *off2,
                deref: *deref,
                deref_first: *deref_first,
                deref_last: *deref_last,
                pty: pty.clone(),
                proj_loud: false,
            };
            let mut loads = vec![format!("{} = load {}+0 as ptr", ptr_temp, spill_slot(place))];
            loads.append(&mut crate::render_util::field_value_into(&proj_op, &field_temp, bid, idx));
            Some((plain_copy(&field_temp), loads, Some(field_temp)))
        }
        _ => Some((op.clone(), vec![], None)),
    }
}

/// A plain (projectionless) Copy of a phi temp: after field extraction
/// the rewritten operand is a value, so render treats it as a move.
fn plain_copy(place: &str) -> Operand {
    Operand::Copy {
        place: place.to_string(),
        off: 0,
        off2: 0,
        deref: false,
        deref_first: false,
        deref_last: false,
        pty: None,
        proj_loud: false,
    }
}

/// Rewrite one rvalue's phi items to slot loads. Returns None when any
/// phi item is unresolvable (caller keeps the original loud path for the
/// whole rvalue — never a partial rewrite). Otherwise returns the new
/// rvalue, load lines to emit before it, and temps to free after it
/// (reads only; moved and borrowed temps are excluded by construction).
pub fn rewrite_rvalue(
    rv: &Rvalue,
    phi: &BTreeMap<String, String>,
    bid: &str,
    idx: &mut usize,
    ref_ok: bool,
) -> Option<(Rvalue, Vec<String>, Vec<String>)> {
    let mut loads: Vec<String> = vec![];
    let mut frees: Vec<String> = vec![];
    // Rewrite one operand, collecting its load and (when `free_it`) its temp.
    let mut rw = |op: &Operand,
                  free_it: bool,
                  loads: &mut Vec<String>,
                  frees: &mut Vec<String>|
     -> Option<Operand> {
        let (new, loads2, temp) = rewrite_operand(op, phi, bid, idx)?;
        loads.extend(loads2);
        if free_it {
            if let Some(t) = temp {
                frees.push(t);
            }
        }
        Some(new)
    };
    let out = match rv {
        Rvalue::Use { op } => Rvalue::Use { op: rw(op, false, &mut loads, &mut frees)? },
        Rvalue::BinOp { op, left, right } => Rvalue::BinOp {
            op: op.clone(),
            left: Box::new(rw(left, true, &mut loads, &mut frees)?),
            right: Box::new(rw(right, true, &mut loads, &mut frees)?),
        },
        Rvalue::UnOp { op, operand } => Rvalue::UnOp {
            op: op.clone(),
            operand: Box::new(rw(operand, true, &mut loads, &mut frees)?),
        },
        Rvalue::Cast { op, ty, castkind, src_ty } => {
            // A bit-identical (Copy) cast moves the temp; a converting
            // cast reads it; an unlowerable cast never uses it. Only the
            // read/never-used cases free (freeing a moved temp traps).
            let dst = crate::asm::cast_dst_short(ty);
            let is_copy = match (castkind.as_deref(), src_ty.as_deref()) {
                (Some(k), Some(s)) => {
                    matches!(crate::asm::lower_cast(k, s, &dst), Some(crate::asm::CastLower::Copy))
                }
                _ => false,
            };
            Rvalue::Cast {
                op: Box::new(rw(op, !is_copy, &mut loads, &mut frees)?),
                ty: ty.clone(),
                castkind: castkind.clone(),
                src_ty: src_ty.clone(),
            }
        }
        Rvalue::Aggregate { elems, layout } => {
            let multi = elems.len() > 1;
            let mut new_elems = vec![];
            for e in elems {
                new_elems.push(rw(e, multi, &mut loads, &mut frees)?);
            }
            Rvalue::Aggregate { elems: new_elems, layout: layout.clone() }
        }
        Rvalue::Repeat { op, len } => Rvalue::Repeat {
            op: Box::new(rw(op, true, &mut loads, &mut frees)?),
            len: len.clone(),
        },
        Rvalue::Call { func, args, sig } => {
            let mut new_args = vec![];
            for a in args {
                new_args.push(rw(a, true, &mut loads, &mut frees)?);
            }
            Rvalue::Call { func: func.clone(), args: new_args, sig: sig.clone() }
        }
        Rvalue::Ref { place, mut_, via, zst, off, off2, deref, deref_first, deref_last, pty, proj_loud, idx: ridx, idx_size: ridx_size, idx_const: ridx_const, idx_fe: ridx_fe, fat: rfat, .. } => {
            if let Some(base) = phi_place(place) {
                if *mut_ || !ref_ok {
                    return None;
                }
                let ty = phi.get(base)?;
                let temp = format!("_phi_{}_{}", bid, *idx);
                *idx += 1;
                loads.push(format!("{} = load {}+0 as {}", temp, spill_slot(base), ty));
                // Borrowed temp: never freed here (drop glue owns it).
                // Zero-sized borrows never use the place: free the unused load.
                if *zst {
                    frees.push(temp.clone());
                }
                // The phi temp holds the base's current value, so
                // the same projection still applies to it.
                Rvalue::Ref { place: temp, mut_: *mut_, via: via.clone(), zst: *zst, off: *off, off2: *off2, deref: *deref, deref_first: *deref_first, pty: pty.clone(), proj_loud: *proj_loud, deref_last: *deref_last, idx: ridx.clone(), idx_size: *ridx_size, idx_const: *ridx_const, idx_fe: *ridx_fe, fat: *rfat }
            } else {
                rv.clone()
            }
        }
        Rvalue::Discriminant { place } => {
            if let Some(base) = phi_place(place) {
                let ty = phi.get(base)?;
                let temp = format!("_phi_{}_{}", bid, *idx);
                *idx += 1;
                loads.push(format!("{} = load {}+0 as {}", temp, spill_slot(base), ty));
                frees.push(temp.clone());
                Rvalue::Discriminant { place: temp }
            } else {
                rv.clone()
            }
        }
        Rvalue::RawPtr { place, mut_, off, off2, deref, deref_first, deref_last, pty, proj_loud, idx: ridx, idx_size: ridx_size, idx_const: ridx_const, idx_fe: ridx_fe, fat: rfat, .. } => {
            if let Some(base) = phi_place(place) {
                let ty = phi.get(base)?;
                let temp = format!("_phi_{}_{}", bid, *idx);
                *idx += 1;
                loads.push(format!("{} = load {}+0 as {}", temp, spill_slot(base), ty));
                // Plain copy: the use moves the temp.
                // Same projection-preservation rule as Ref above.
                Rvalue::RawPtr { place: temp, mut_: *mut_, off: *off, off2: *off2, deref: *deref, deref_first: *deref_first, pty: pty.clone(), proj_loud: *proj_loud, deref_last: *deref_last, idx: ridx.clone(), idx_size: *ridx_size, idx_const: *ridx_const, idx_fe: *ridx_fe, fat: *rfat }
            } else {
                rv.clone()
            }
        }
        // Version-conflict Unsupported rvalues lost their kind in
        // version.rs (pre-marker shapes): unresolvable, stay loud.
        Rvalue::Unsupported { text } if text.starts_with("version conflict at join") => {
            return None;
        }
        _ => rv.clone(),
    };
    Some((out, loads, frees))
}

/// All block indices whose terminator is Return (for phi_ref_allowed).
pub fn return_blocks(blocks: &[Block]) -> Vec<usize> {
    blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b.terminator, Term::Return { .. }))
        .map(|(i, _)| i)
        .collect()
}


#[cfg(test)]
mod phi_tests {
    use super::*;
    use crate::mir::{Operand, Rvalue};

    fn phi_map() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("_1".to_string(), "i32".to_string()),
            ("_2".to_string(), "ptr".to_string()),
        ])
    }

    fn conflict(place: &str) -> Operand {
        Operand::Conflict {
            place: place.to_string(),
            off: 0,
            off2: 0,
            deref: false,
            deref_first: false,
            deref_last: false,
            pty: None,
            proj_loud: false,
        }
    }

    fn conflict_proj(place: &str, off: u64, pty: &str) -> Operand {
        Operand::Conflict {
            place: place.to_string(),
            off,
            off2: 0,
            deref: false,
            deref_first: false,
            deref_last: false,
            pty: Some(pty.to_string()),
            proj_loud: false,
        }
    }

    fn const_op(v: &str) -> Operand {
        Operand::Const { value: v.to_string(), str_bytes: None, str_len: None }
    }

    #[test]
    fn slot_ty_maps() {
        assert_eq!(phi_slot_ty("i32"), Some("i32".to_string()));
        assert_eq!(phi_slot_ty("bool"), Some("u8".to_string()));
        assert_eq!(phi_slot_ty("usize"), Some("u64".to_string()));
        assert_eq!(phi_slot_ty("&'{erased} mut Frame"), Some("ptr".to_string()));
        assert_eq!(phi_slot_ty("*mut u8"), Some("ptr".to_string()));
        assert_eq!(phi_slot_ty("std::ptr::NonNull<u8>"), Some("ptr".to_string()));
        assert_eq!(phi_slot_ty("f64"), None);
        assert_eq!(phi_slot_ty("String"), None);
        assert_eq!(phi_slot_ty("std::future::ResumeTy"), None);
    }

    #[test]
    fn marker_roundtrip() {
        assert_eq!(phi_marker("_23"), "__PHI__:_23");
        assert_eq!(phi_place("__PHI__:_23"), Some("_23"));
        assert_eq!(phi_place("_23_v0"), None);
    }

    #[test]
    fn use_rewrite_moves_temp() {
        let rv = Rvalue::Use { op: conflict("_1") };
        let mut idx = 0;
        let (out, loads, frees) = rewrite_rvalue(&rv, &phi_map(), "bb1", &mut idx, true).unwrap();
        assert_eq!(loads, vec!["_phi_bb1_0 = load _1_spill+0 as i32"]);
        assert!(frees.is_empty(), "plain-assign use moves the temp");
        assert!(matches!(out, Rvalue::Use { op: Operand::Copy { place , .. } } if place == "_phi_bb1_0"));
    }

    #[test]
    fn binop_rewrite_frees_temps() {
        let rv = Rvalue::BinOp {
            op: "AddWithOverflow".to_string(),
            left: Box::new(conflict("_1")),
            right: Box::new(Operand::Const {
                value: "1_i32".to_string(),
                str_bytes: None,
                str_len: None,
            }),
        };
        let mut idx = 0;
        let (_, loads, frees) = rewrite_rvalue(&rv, &phi_map(), "bb2", &mut idx, true).unwrap();
        assert_eq!(loads.len(), 1);
        assert_eq!(frees, vec!["_phi_bb2_0"]);
    }

    #[test]
    fn mut_ref_stays_loud() {
        let rv = Rvalue::Ref { place: "__PHI__:_2".to_string(), mut_: true, via: None, zst: false , off: 0, off2: 0, deref: false, deref_first: false, proj_loud: false , pty: None , deref_last: false , idx: None, idx_size: 0, idx_const: None, idx_fe: None, fat: false};
        let mut idx = 0;
        assert!(rewrite_rvalue(&rv, &phi_map(), "bb3", &mut idx, true).is_none());
    }

    #[test]
    fn shared_ref_needs_dominating_block() {
        let rv = Rvalue::Ref { place: "__PHI__:_2".to_string(), mut_: false, via: None, zst: false , off: 0, off2: 0, deref: false, deref_first: false, proj_loud: false , pty: None , deref_last: false , idx: None, idx_size: 0, idx_const: None, idx_fe: None, fat: false};
        let mut idx = 0;
        assert!(rewrite_rvalue(&rv, &phi_map(), "bb3", &mut idx, false).is_none());
        let mut idx = 0;
        let (out, loads, frees) =
            rewrite_rvalue(&rv, &phi_map(), "bb3", &mut idx, true).unwrap();
        assert_eq!(loads.len(), 1);
        assert!(frees.is_empty(), "borrowed temp is drop glue's");
        assert!(matches!(out, Rvalue::Ref { place, .. } if place == "_phi_bb3_0"));
    }

    #[test]
    fn unknown_type_stays_loud() {
        let rv = Rvalue::Use { op: conflict("_9") };
        let mut idx = 0;
        assert!(rewrite_rvalue(&rv, &phi_map(), "bb1", &mut idx, true).is_none());
    }

    #[test]
    fn projected_buffer_field_load_extracts_in_loads() {
        // join-05 shape: _4 is a buffer (ptr slot), read at field 1.
        let phi = BTreeMap::from([("_4".to_string(), "ptr".to_string())]);
        let rv = Rvalue::Use { op: conflict_proj("_4", 8, "u64") };
        let mut idx = 0;
        let (out, loads, frees) = rewrite_rvalue(&rv, &phi, "bb4", &mut idx, true).unwrap();
        assert_eq!(loads, vec![
            "_phi_bb4_0 = load _4_spill+0 as ptr",
            "_phi_bb4_1 = load _phi_bb4_0+8 as u64",
        ]);
        assert!(frees.is_empty(), "plain-assign use moves the field temp");
        assert!(matches!(out, Rvalue::Use { op: Operand::Copy { place, .. } } if place == "_phi_bb4_1"));
    }

    #[test]
    fn projected_buffer_field0_load_extracts_in_loads() {
        let phi = BTreeMap::from([("_4".to_string(), "ptr".to_string())]);
        let rv = Rvalue::Use { op: conflict_proj("_4", 0, "u64") };
        let mut idx = 0;
        let (out, loads, _) = rewrite_rvalue(&rv, &phi, "bb4", &mut idx, true).unwrap();
        assert_eq!(loads, vec![
            "_phi_bb4_0 = load _4_spill+0 as ptr",
            "_phi_bb4_1 = load _phi_bb4_0+0 as u64",
        ]);
        assert!(matches!(out, Rvalue::Use { op: Operand::Copy { place, .. } } if place == "_phi_bb4_1"));
    }

    #[test]
    fn buf_field_copy_stays_loud() {
        let phi = BTreeMap::from([("_4".to_string(), "ptr".to_string())]);
        let rv = Rvalue::Use { op: conflict_proj("_4", 8, "buf") };
        let mut idx = 0;
        assert!(rewrite_rvalue(&rv, &phi, "bb4", &mut idx, true).is_none());
    }

    #[test]
    fn scalar_slot_projected_conflict_stays_loud() {
        let rv = Rvalue::Use { op: conflict_proj("_1", 8, "u64") };
        let mut idx = 0;
        assert!(rewrite_rvalue(&rv, &phi_map(), "bb4", &mut idx, true).is_none());
    }

    #[test]
    fn untyped_projection_stays_loud() {
        let phi = BTreeMap::from([("_4".to_string(), "ptr".to_string())]);
        let op = Operand::Conflict {
            place: "_4".to_string(),
            off: 8,
            off2: 0,
            deref: false,
            deref_first: false,
            deref_last: false,
            pty: None,
            proj_loud: false,
        };
        let rv = Rvalue::Use { op };
        let mut idx = 0;
        assert!(rewrite_rvalue(&rv, &phi, "bb4", &mut idx, true).is_none());
    }

    #[test]
    fn driver_flagged_projection_stays_loud() {
        let phi = BTreeMap::from([("_4".to_string(), "ptr".to_string())]);
        let mut op = conflict_proj("_4", 8, "u64");
        if let Operand::Conflict { proj_loud, .. } = &mut op {
            *proj_loud = true;
        }
        let rv = Rvalue::Use { op };
        let mut idx = 0;
        assert!(rewrite_rvalue(&rv, &phi, "bb4", &mut idx, true).is_none());
    }

    #[test]
    fn build_phi_gives_buffer_base_a_ptr_slot() {
        // join-05 shape, versioned: both _4 defs are 2-elem aggregates.
        let agg = |a: &str, b: &str| Rvalue::Aggregate {
            elems: vec![const_op(a), const_op(b)],
            layout: Some(crate::mir::AdtLayout { size: 16, offsets: vec![0, 8] }),
        };
        let blocks = vec![
            crate::mir::Block {
                id: "bb0".to_string(),
                statements: vec![crate::mir::Stmt::Assign {
                    dest: "_4_v0".to_string(),
                    dest_place: Some("_4".to_string()),
                    rvalue: agg("11", "22"),
                }],
                terminator: crate::mir::Term::Goto { target: "bb2".to_string() },
            },
            crate::mir::Block {
                id: "bb1".to_string(),
                statements: vec![crate::mir::Stmt::Assign {
                    dest: "_4_v1".to_string(),
                    dest_place: Some("_4".to_string()),
                    rvalue: agg("33", "44"),
                }],
                terminator: crate::mir::Term::Goto { target: "bb2".to_string() },
            },
            crate::mir::Block {
                id: "bb2".to_string(),
                statements: vec![crate::mir::Stmt::Assign {
                    dest: "_0".to_string(),
                    dest_place: Some("_0".to_string()),
                    rvalue: Rvalue::Use { op: conflict_proj("_4", 8, "u64") },
                }],
                terminator: crate::mir::Term::Return { ret: Some("_0".to_string()) },
            },
        ];
        let local_types = BTreeMap::from([
            ("_4".to_string(), "P".to_string()),
            ("_0".to_string(), "u64".to_string()),
        ]);
        let phi = build_phi(&blocks, &local_types, Some("u64"), &BTreeMap::new(), &HashSet::from([0, 1, 2]));
        assert_eq!(phi.get("_4").map(|s| s.as_str()), Some("ptr"));
    }

    #[test]
    fn build_phi_ignores_place_stores_when_detecting_buffers() {
        // One whole aggregate def plus a place store through the same
        // base: the store must not disqualify the buffer base.
        let agg = Rvalue::Aggregate {
            elems: vec![const_op("11"), const_op("22")],
            layout: Some(crate::mir::AdtLayout { size: 16, offsets: vec![0, 8] }),
        };
        let blocks = vec![
            crate::mir::Block {
                id: "bb0".to_string(),
                statements: vec![crate::mir::Stmt::Assign {
                    dest: "_4_v0".to_string(),
                    dest_place: Some("_4".to_string()),
                    rvalue: agg,
                }],
                terminator: crate::mir::Term::Goto { target: "bb1".to_string() },
            },
            crate::mir::Block {
                id: "bb1".to_string(),
                statements: vec![crate::mir::Stmt::Assign {
                    dest: "_4_v0".to_string(),
                    dest_place: Some("(_4_v0.1: u64)".to_string()),
                    rvalue: Rvalue::Use { op: const_op("99") },
                }],
                terminator: crate::mir::Term::Goto { target: "bb2".to_string() },
            },
            crate::mir::Block {
                id: "bb2".to_string(),
                statements: vec![crate::mir::Stmt::Assign {
                    dest: "_0".to_string(),
                    dest_place: Some("_0".to_string()),
                    rvalue: Rvalue::Use { op: conflict_proj("_4", 8, "u64") },
                }],
                terminator: crate::mir::Term::Return { ret: Some("_0".to_string()) },
            },
        ];
        let local_types = BTreeMap::from([
            ("_4".to_string(), "P".to_string()),
            ("_0".to_string(), "u64".to_string()),
        ]);
        let phi = build_phi(&blocks, &local_types, Some("u64"), &BTreeMap::new(), &HashSet::from([0, 1, 2]));
        assert_eq!(phi.get("_4").map(|s| s.as_str()), Some("ptr"));
    }
}
