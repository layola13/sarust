//! mir2sa::spill — reload slots for multi-use values.
//!
//! SA `=` moves (single-assignment): `_X = Copy _Y` traps UseAfterMove when
//! `_Y` is read again later. Shared reads inside call args / BinOp operands /
//! stores / eq are fine (non-consuming) — only plain-assign copies need help.
//!
//! For values whose TYPE IS KNOWN, spill to a slot once and reload per copy:
//! - synthetic buffers (`_agg_*`, `_rep_*`): always `ptr` (we generate them);
//! - call dests with resolvable sig ret: mapped store-ty (u64/i64/u32/...).
//! - locals/params with a driver local type (phi-11): the sars driver
//!   emits every MIR local's type, so scalar/pointer Copy sources spill
//!   exactly instead of rendering Copy as an SA move (PhiStateConflict).
//! Moves stay direct (single exact transfer); only Copy-uses reload, so the
//! transform is order-independent (loads never touch the source reg).
//! Slots are always 8 bytes (all store-tys fit; slack is harmless).
//! Everything else (untyped ADTs, 128-bit ints, BinOp-defined values)
//! stays on the old paths — never a silent slot of the wrong width.

use std::collections::{BTreeMap, BTreeSet};

use crate::asm::scalar_width_bits;
use crate::mir::{Operand, Rvalue, Stmt, Term};

/// Spill slots: base/dest reg -> SA store type (`ptr`, `u64`, `i32`, …).
pub type SpillMap = BTreeMap<String, String>;

/// SA store type for a call-result spill slot, from the callee sig ret.
/// None when unmappable (caller goes loud through existing paths).
pub fn call_spill_ty(ret: &str) -> Option<String> {
    let t = ret.trim();
    if scalar_width_bits(t).is_some() {
        return Some(if t == "bool" {
            "u8".to_string()
        } else if t == "usize" {
            "u64".to_string()
        } else if t == "isize" {
            "i64".to_string()
        } else {
            t.to_string()
        });
    }
    if t == "ptr" || t.starts_with('*') || t.starts_with('&') {
        return Some("ptr".to_string());
    }
    None
}

/// SA store type for a cast dest's spill slot, from the MIR target type.
/// Pointer spellings collapse to `ptr`, scalars map through `sa_scalar_ty`
/// (bool->u8, usize->u64). None when the cast does not lower to a scalar
/// (struct/enum/fn targets: no SA slot type) — caller stays direct.
pub fn cast_spill_ty(ty: &str) -> Option<String> {
    let dst = crate::asm::cast_dst_short(ty);
    if dst == "ptr" {
        return Some("ptr".to_string());
    }
    crate::const_util::sa_scalar_ty(dst.trim());
    crate::asm::scalar_width_bits(&dst)
        .filter(|w| *w != 128)
        .map(|_| crate::const_util::sa_scalar_ty(&dst).to_string())
}

/// Copy-use positions that need reload (plain-assign moves of shared reads):
/// Assign-Use RHS, single-elem aggregate elems, and cast operands. All other
/// Copy positions render shared reads inline (non-consuming) and need nothing.
/// Cast operands matter for pointer-copy chains (Box deref null/align checks
/// copy the same pointer 2-3 times — corpus f_box/f_raw/f_underscore).
fn copy_use_targets(st: &Stmt) -> Vec<String> {
    match st {
        Stmt::Assign { rvalue, .. } => match rvalue {
            Rvalue::Use { op } => match op {
                Operand::Copy { place, off, pty, .. } if *off == 0 && pty.as_deref() != Some("buf") => vec![place.clone()],
                _ => vec![],
            },
            Rvalue::Aggregate { elems, .. } if elems.len() == 1 => match &elems[0] {
                Operand::Copy { place, off, pty, .. } if *off == 0 && pty.as_deref() != Some("buf") => vec![place.clone()],
                _ => vec![],
            },
            Rvalue::Cast { op, .. } => match op.as_ref() {
                Operand::Copy { place, off, pty, .. } if *off == 0 && pty.as_deref() != Some("buf") => vec![place.clone()],
                _ => vec![],
            },
            _ => vec![],
        },
        _ => vec![],
    }
}

/// Positions that CONSUME their operand reg (plain assign of one place).
/// MIR Move on a Copy-typed local still leaves the local valid, but SA `=`
/// moves, so the reg would be gone before its own `!b` release.
fn consuming_use_targets(st: &Stmt) -> Vec<String> {
    match st {
        Stmt::Assign { rvalue, .. } => match rvalue {
            Rvalue::Use { op } => match op {
                Operand::Move { place, off, pty, .. } if *off == 0 && pty.as_deref() != Some("buf") => vec![place.clone()],
                _ => vec![],
            },
            Rvalue::Aggregate { elems, .. } if elems.len() == 1 => match &elems[0] {
                Operand::Move { place, off, pty, .. } if *off == 0 && pty.as_deref() != Some("buf") => vec![place.clone()],
                _ => vec![],
            },
            Rvalue::Cast { op, .. } => match op.as_ref() {
                Operand::Move { place, off, pty, .. } if *off == 0 && pty.as_deref() != Some("buf") => vec![place.clone()],
                _ => vec![],
            },
            _ => vec![],
        },
        _ => vec![],
    }
}

/// Build the spill map for one function: synth bases (`_agg_*`, `_rep_*`
/// defs) and call dests (with mappable sig ret) that have at least one
/// Copy-use elsewhere in the function.
///
/// Soundness gates (violations trap downstream instead):
/// - SINGLE-DEF only: multi-def regs need versioning (each def would emit
///   its own slot setup = slot redefinition; uses can't pick a slot).
///   Multi-def falls back to rebind-loud/UAM paths (honest, counted).
/// - Copy-uses only (moves stay direct single transfers).
// True when a cast lowers to a real value: the existing lower_cast
// verdict, or a slice-unsizing cast whose length comes from the source
// local's driver type (see asm::unsize_slice_len). The truncated
// `src_ty` field alone cannot decide the unsizing case.
fn cast_lowers(
    ty: &str,
    castkind: &Option<String>,
    src_ty: &Option<String>,
    op: &Operand,
    local_types: &BTreeMap<String, String>,
) -> bool {
    let dst = crate::asm::cast_dst_short(ty);
    if let (Some(k), Some(s)) = (castkind.as_deref(), src_ty.as_deref()) {
        if crate::asm::lower_cast(k, s, &dst).is_some() {
            return true;
        }
        if let Operand::Move { place , .. } | Operand::Copy { place , .. } = op {
            if let Some(t) = local_types
                .get(place)
                .or_else(|| local_types.get(&strip_version(place)))
            {
                return crate::asm::unsize_slice_len(k, t, ty).is_some();
            }
        }
    }
    false
}

pub fn build_spill(
    blocks: &[crate::mir::Block],
    local_types: &BTreeMap<String, String>,
) -> SpillMap {
    // Copy-use targets across the whole function.
    let mut copies: BTreeSet<String> = BTreeSet::new();
    // Consuming (Move) uses: these need a slot for the SOURCE to survive.
    let mut consuming: BTreeSet<String> = BTreeSet::new();
    // SetDisc places: `store place+0` reads the base register, so a place
    // consumed by an earlier plain-assign Copy/Move traps UseAfterMove at
    // the store. Counted here alongside the plain-assign uses.
    let mut setdisc: BTreeSet<String> = BTreeSet::new();
    for b in blocks {
        for st in &b.statements {
            copies.extend(copy_use_targets(st));
            consuming.extend(consuming_use_targets(st));
            if let Stmt::SetDisc { place, .. } = st {
                setdisc.insert(place.clone());
            }
        }
    }
    if copies.is_empty() && consuming.is_empty() {
        return BTreeMap::new();
    }
    // Definition counts (Assign dests + Call dests): only single-def regs
    // may spill (multi-def needs versioning; per-def slot setups would
    // redefine the slot and uses couldn't pick one).
    let mut def_count: BTreeMap<String, usize> = BTreeMap::new();
    for b in blocks {
        for st in &b.statements {
            // Place stores are not definitions (version.rs): a store
            // through a base must not break the base's single-def
            // status (118: init + store counted twice, killing the
            // static-address slot and trapping UseAfterMove on later
            // copies; 192: a param store broke the param branch).
            // NOTE: this runs on VERSIONED blocks, where a normal
            // def's dest_place is stale (versioning renames dest
            // but not dest_place: dest=_1_v1, dest_place=_1), so
            // `dest_place != dest` would misclassify every
            // versioned normal def as a store (22/103/09: _1_v1,
            // _4_v1, _24_v0 lost their slots). A store place
            // carries structure (parens/deref/fields); a normal
            // def's dest_place is a plain local token.
            if let Stmt::Assign { dest, dest_place, .. } = st {
                if dest_place.as_deref().is_some_and(|dp| {
                    let t = dp.trim();
                    !(t.len() > 1
                        && t.starts_with('_')
                        && t[1..]
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_'))
                }) {
                    continue;
                }
                *def_count.entry(dest.clone()).or_insert(0) += 1;
            }
        }
        if let Term::Call { dest: Some(d), .. } = &b.terminator {
            *def_count.entry(d.clone()).or_insert(0) += 1;
        }
    }
    let single = |r: &str| def_count.get(r).copied().unwrap_or(0) == 1;
    let mut map = SpillMap::new();
    for b in blocks {
        for st in &b.statements {
            if let Stmt::Assign { dest, rvalue: Rvalue::Call { sig, .. }, .. } = st {
                if copies.contains(dest) && single(dest) {
                    if let Some(s) = sig {
                        if let Some(t) = call_spill_ty(&s.ret) {
                            map.insert(dest.clone(), t);
                        }
                    }
                }
            }
        }
        if let Term::Call { dest: Some(d), sig, .. } = &b.terminator {
            if copies.contains(d) && single(d) {
                if let Some(s) = sig {
                    if let Some(t) = call_spill_ty(&s.ret) {
                        map.insert(d.clone(), t);
                    }
                }
            }
        }
    }
    // Synthetic bases: any `_agg_*`/`_rep_*` reg with a Copy-use spills
    // as `ptr` (we generate these buffers; always pointer-valued).
    for c in &copies {
        if (c.starts_with("_agg_") || c.starts_with("_rep_")) && !map.contains_key(c) {
            map.insert(c.clone(), "ptr".to_string());
        }
    }
    // Cast dests: pointer copies (Box deref null/align chains copy the same
    // pointer several times). Only casts that lower to a real value get a
    // slot — a loud cast binds a `0` placeholder and gains nothing.
    for b in blocks {
        for st in &b.statements {
            if let Stmt::Assign { dest, rvalue: Rvalue::Cast { ty, castkind, src_ty, op, .. }, .. } = st {
                if copies.contains(dest) && single(dest) && !map.contains_key(dest) {
                    let lowers = cast_lowers(ty, castkind, src_ty, op, local_types);
                    if lowers {
                        if let Some(t) = cast_spill_ty(ty) {
                            map.insert(dest.clone(), t);
                        }
                    }
                }
            }
        }
    }
    // Single-elem aggregate propagation: `_d = Agg([Move|Copy _p])` where
    // `_p` is pointer-valued makes `_d` pointer-valued too. The wrapper
    // around a NonNull cast is the motivating shape (09: `_24` wraps `_25`,
    // a cast to NonNull<Context>): the cast itself has no Copy use, so
    // the cast loop above never spills it, and without propagation `_24`
    // gets no slot either. Its Move use in bb19 then consumes `_24_v0` on
    // one path only, which traps PhiStateConflict at the bb4 join. Only
    // casts that lower to a real value feed the map (a loud cast binds a
    // `0` placeholder and gains nothing), and dests need the same gates
    // as borrow dests: single-def (versioned names are, by construction)
    // and a Copy or consuming use. Run to fixpoint so chains of wrappers
    // propagate (aggregate of aggregate, or of an already-spilled reg).
    let mut cast_ty: BTreeMap<String, String> = BTreeMap::new();
    for b in blocks {
        for st in &b.statements {
            if let Stmt::Assign { dest, rvalue: Rvalue::Cast { ty, castkind, src_ty, op, .. }, .. } = st {
                let lowers = cast_lowers(ty, castkind, src_ty, op, local_types);
                if lowers {
                    if let Some(t) = cast_spill_ty(ty) {
                        cast_ty.insert(dest.clone(), t);
                    }
                }
            }
        }
    }
    loop {
        let mut changed = false;
        for b in blocks {
            for st in &b.statements {
                if let Stmt::Assign { dest, rvalue: Rvalue::Aggregate { elems, .. }, .. } = st {
                    if elems.len() != 1 || map.contains_key(dest) || !single(dest) {
                        continue;
                    }
                    let place = match &elems[0] {
                        Operand::Move { place , .. } | Operand::Copy { place , .. } => place,
                        _ => continue,
                    };
                    if !(copies.contains(dest) || consuming.contains(dest)) {
                        continue;
                    }
                    let t = cast_ty
                        .get(place)
                        .cloned()
                        .or_else(|| map.get(place).cloned());
                    if let Some(t) = t {
                        map.insert(dest.clone(), t);
                        changed = true;
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Borrow dests: `b = &p` is pointer-valued, and a plain assign of `b`
    // (Aggregate/Use/Cast RHS) CONSUMES the reg. The later release (`drop(b)`
    // or the borrow-end insertion before `drop(p)`) then reads a reg the
    // Referee no longer has — UnknownRegister, not UseAfterMove. Spilling
    // keeps `b` bound and reloads at the consuming use. ZST borrows emit
    // `= 0` (no pointer) and are skipped.
    for b in blocks {
        for st in &b.statements {
            if let Stmt::Assign { dest, rvalue: Rvalue::Ref { zst: false, .. }, .. } = st {
                let used = copies.contains(dest) || consuming.contains(dest);
                if used && single(dest) && !map.contains_key(dest) {
                    map.insert(dest.clone(), "ptr".to_string());
                }
            }
        }
    }
    // SetDisc places: single-def places with a plain-assign Copy/Move use
    // spill as `ptr` (enum bases are addresses). Multi-def places are
    // versioned instead (excluded by the single-def gate), and places with
    // no plain-assign use need no slot (nothing moves them before the
    // store). Already-mapped defs keep their own slot type.
    for p in &setdisc {
        if single(p) && (copies.contains(p) || consuming.contains(p)) && !map.contains_key(p) {
            map.insert(p.clone(), "ptr".to_string());
        }
    }
    // Static-address const dests (`_x = Val(Scalar(allocN), *mut T)`):
    // the address is not materializable, so the def goes loud with a `0`
    // placeholder - but the TYPE is known (thin raw pointer, via
    // const_elem_ty), so a slot keeps multi-use copies alive exactly
    // like the byte-literal branch below: the loud path mirrors the
    // placeholder into the slot (spill_placeholder) and Cast/Use copies
    // reload from it instead of moving the placeholder twice. Without
    // the slot the second plain-assign copy traps UseAfterMove (118:
    // `_35 = _1_v0` consumes, then `_40 = _1_v0` traps).
    // Only `*` raw-pointer shapes (const_elem_ty "ptr"): `&` refs may be
    // fat and stay untyped there, so they never reach this branch.
    for b in blocks {
        for st in &b.statements {
            if let Stmt::Assign {
                dest,
                rvalue: Rvalue::Use { op: Operand::Const { value, str_bytes: None, .. } },
                ..
            } = st
            {
                let is_ptr =
                    crate::const_util::const_elem_ty(value) == Some(("ptr", 8));
                let used = copies.contains(dest) || consuming.contains(dest);
                if is_ptr && used && single(dest) && !map.contains_key(dest) {
                    map.insert(dest.clone(), "ptr".to_string());
                }
            }
        }
    }
    // Byte-literal const dests (`_x = <&[u8; K] payload>`): the value is a
    // buffer address (known `ptr`). MIR Copy-uses of a Copy-typed local lower
    // to plain assigns, which consume the reg — a later `Copy` (or the
    // borrow-end release) would then read a reg the Referee dropped.
    // Slice-literal dests (`_x = <&str payload>`) join this branch too:
    // the pair planner binds the (ptr,len) pair's address, which is
    // likewise a thin `ptr`, so Copy uses reload it the same way.
    for b in blocks {
        for st in &b.statements {
            if let Stmt::Assign { dest, rvalue: Rvalue::Use { op: Operand::Const { value, str_bytes, str_len } }, .. } = st {
                let inlineable =
                    crate::layout::plan_const_bytes(dest, "bb", 0, value, str_bytes.as_ref(), *str_len)
                        .is_some()
                        || crate::layout::plan_str_pair(dest, "bb", 0, value, str_bytes.as_ref(), *str_len)
                            .is_some();
                let used = copies.contains(dest) || consuming.contains(dest);
                if inlineable && used && single(dest) && !map.contains_key(dest) {
                    map.insert(dest.clone(), "ptr".to_string());
                }
            }
        }
    }
    // Driver-typed locals/params (phi-11): a Copy source whose MIR type
    // maps to an SA store type spills exactly, so its Copy uses reload
    // instead of moving it (a move on one arm of a join traps
    // PhiStateConflict, and a move of a value still used after the join
    // traps UseAfterMove). Gates, all conservative:
    // - the reg has a Copy use (plain-assign Copy is the only consuming
    //   Copy position; reads in call args / binops never move);
    // - single definition (versioned names are, by construction), or a
    //   header-bound param (no statement def, `_N` shape, never an asm
    //   out — asm outs are defs too, just not counted in def_count);
    // - the defining rvalue sets its slot up when rendered (Use / Call /
    //   Cast / Ref / Aggregate / BinOp / UnOp arms do; Discriminant /
    //   RawPtr arms do not, so those defs stay on the old paths rather
    //   than reloading from a slot that was never stored);
    // - the driver type maps to a real store type (scalar or ptr; ADTs,
    //   slices, tuples and 128-bit ints map to None and stay as today).
    // Params get their slot setup at the entry block (see lower.rs);
    // statement/call dests get it from their own render arm.
    if !local_types.is_empty() {
        // Defining-rvalue kinds that set a slot up (see render.rs).
        let mut setup_kind: BTreeMap<String, bool> = BTreeMap::new();
        let mut asm_outs: BTreeSet<String> = BTreeSet::new();
        for b in blocks {
            for st in &b.statements {
                if let Stmt::Assign { dest, rvalue, .. } = st {
                    let ok = matches!(
                        rvalue,
                        Rvalue::Use { .. }
                            | Rvalue::Call { .. }
                            | Rvalue::Cast { .. }
                            | Rvalue::Ref { .. }
                            | Rvalue::Aggregate { .. }
                            | Rvalue::BinOp { .. }
                            | Rvalue::UnOp { .. }
                    );
                    setup_kind.insert(dest.clone(), ok);
                }
            }
            match &b.terminator {
                Term::Call { dest: Some(d), .. } => {
                    setup_kind.insert(d.clone(), true);
                }
                Term::InlineAsm { outs, .. } => {
                    for o in outs {
                        asm_outs.insert(o.clone());
                    }
                }
                _ => {}
            }
        }
        for c in &copies {
            if map.contains_key(c) {
                continue;
            }
            let base = strip_version(c);
            let Some(ty) = local_types.get(&base) else {
                continue;
            };
            let Some(t) = cast_spill_ty(ty) else {
                continue;
            };
            let is_param = def_count.get(c).copied().unwrap_or(0) == 0
                && !asm_outs.contains(c)
                && is_plain_local(c);
            let ok = if is_param {
                // Header-bound: no statement def, so no setup-kind gate;
                // lower.rs emits the slot setup at the entry block.
                true
            } else {
                single(c) && setup_kind.get(c).copied().unwrap_or(false)
            };
            if ok {
                map.insert(c.clone(), t);
            }
        }
        // Aggregate-root spill (uam-tail): the driver collapses every
        // MIR subplace to its root local, so a field Copy or Move of a
        // ptr-represented aggregate renders as a whole-root SA move.
        // The first field access consumes the root and every later
        // field access traps UseAfterMove, in the same block or later.
        // A spilled root reloads its buffer pointer per plain-assign
        // use and never consumes the root, so later field accesses
        // stay legal. Value fidelity is unchanged from today: field
        // offsets are not in mir.json, and the reload binds the same
        // buffer pointer the plain move bound before.
        // Gates, all conservative: the base local type maps to no SA
        // store type (scalars and pointers are handled above), it is
        // not unit, the reg has a plain-assign use and at least two
        // uses in total, it is never a Drop place, and its def binds
        // a pointer in SA (aggregate param, multi-elem Aggregate,
        // ptr-returning Call, Ref, ptr Cast, Repeat, or a Use of a
        // pointer-valued source). Versioned scalar field defs such as
        // a version bound from an i32 temp are excluded by the def
        // gate even though their base local is an aggregate type.
        // Single-def or header-bound param, and the def kind must set
        // a slot up (render sets the multi-elem Aggregate dest slot
        // up alongside the synthetic base slot).
        {
            let mut total_uses: BTreeMap<String, usize> = BTreeMap::new();
            let mut drop_places: BTreeSet<String> = BTreeSet::new();
            let mut def_ptr: BTreeMap<String, bool> = BTreeMap::new();
            for b in blocks {
                for st in &b.statements {
                    match st {
                        Stmt::Assign { dest, rvalue, .. } => {
                            let p = match rvalue {
                                Rvalue::Aggregate { elems, .. } => {
                                    if elems.len() >= 2 {
                                        true
                                    } else if elems.len() == 1 {
                                        match &elems[0] {
                                            Operand::Move { place , .. } | Operand::Copy { place , .. } => ptr_operand(place, &map, local_types),
                                            _ => false,
                                        }
                                    } else {
                                        false
                                    }
                                }
                                Rvalue::Call { sig, .. } => match sig {
                                    Some(s) => s.ret == "ptr",
                                    None => false,
                                },
                                Rvalue::Ref { zst, .. } => !*zst,
                                Rvalue::Cast { ty, .. } => cast_spill_ty(ty).is_some_and(|x| x == "ptr"),
                                Rvalue::Use { op } => match op {
                                    Operand::Move { place , .. } | Operand::Copy { place , .. } => ptr_operand(place, &map, local_types),
                                    _ => false,
                                },
                                Rvalue::Repeat { .. } => true,
                                _ => false,
                            };
                            def_ptr.insert(dest.clone(), p);
                            match rvalue {
                                Rvalue::Use { op } => count_operand(&mut total_uses, op),
                                Rvalue::Aggregate { elems, .. } => {
                                    for e in elems {
                                        count_operand(&mut total_uses, e);
                                    }
                                }
                                Rvalue::BinOp { left, right, .. } => {
                                    count_operand(&mut total_uses, left);
                                    count_operand(&mut total_uses, right);
                                }
                                Rvalue::UnOp { operand, .. } => {
                                    count_operand(&mut total_uses, operand);
                                }
                                Rvalue::Cast { op, .. } => {
                                    count_operand(&mut total_uses, op);
                                }
                                Rvalue::Call { args, .. } => {
                                    for a in args {
                                        count_operand(&mut total_uses, a);
                                    }
                                }
                                Rvalue::Repeat { op, .. } => {
                                    count_operand(&mut total_uses, op);
                                }
                                Rvalue::Ref { place, .. } | Rvalue::Discriminant { place } | Rvalue::RawPtr { place, .. } => {
                                    count_place(&mut total_uses, place);
                                }
                                _ => {}
                            }
                        }
                        Stmt::SetDisc { place, .. } => {
                            count_place(&mut total_uses, place);
                        }
                        _ => {}
                    }
                }
                match &b.terminator {
                    Term::Call { dest: Some(d), sig, args, .. } => {
                        let p = match sig {
                            Some(s) => s.ret == "ptr",
                            None => false,
                        };
                        def_ptr.entry(d.clone()).or_insert(p);
                        for a in args {
                            count_operand(&mut total_uses, a);
                        }
                    }
                    Term::Call { args, .. } => {
                        for a in args {
                            count_operand(&mut total_uses, a);
                        }
                    }
                    Term::Drop { place, .. } => {
                        count_place(&mut total_uses, place);
                        drop_places.insert(place.clone());
                    }
                    Term::SwitchInt { discr, .. } => {
                        count_operand(&mut total_uses, discr);
                    }
                    Term::Assert { cond, .. } => {
                        count_operand(&mut total_uses, cond);
                    }
                    Term::InlineAsm { ins, .. } => {
                        for a in ins {
                            count_operand(&mut total_uses, a);
                        }
                    }
                    _ => {}
                }
            }
            let plain: BTreeSet<String> = copies.union(&consuming).cloned().collect();
            for r in &plain {
                if map.contains_key(r) {
                    continue;
                }
                if drop_places.contains(r) {
                    continue;
                }
                if total_uses.get(r).copied().unwrap_or(0) < 2 {
                    continue;
                }
                let base = strip_version(r);
                let Some(ty) = local_types.get(&base) else {
                    continue;
                };
                if ty.trim() == "()" {
                    continue;
                }
                if cast_spill_ty(ty).is_some() {
                    continue;
                }
                let is_param = def_count.get(r).copied().unwrap_or(0) == 0 && !asm_outs.contains(r) && is_plain_local(r);
                let ok = if is_param {
                    true
                } else {
                    single(r) && setup_kind.get(r).copied().unwrap_or(false)
                };
                if !ok {
                    continue;
                }
                let binds_ptr = if is_param {
                    true
                } else {
                    def_ptr.get(r).copied().unwrap_or(false)
                };
                if !binds_ptr {
                    continue;
                }
                map.insert(r.clone(), "ptr".to_string());
            }
        }
    }
    map
}

/// Pointer-valued operand test for the aggregate-root spill gate: a
/// synthetic buffer, an already ptr-spilled reg, or a source whose MIR
/// type maps to no SA store type (aggregate-like, hence ptr in SA).
/// Scalar and pointer types return false here: pointers are already
/// handled by the phi-11 branch, scalars are not ptr-represented.
fn ptr_operand(place: &str, map: &SpillMap, local_types: &BTreeMap<String, String>) -> bool {
    if place.starts_with("_agg_") || place.starts_with("_rep_") {
        return true;
    }
    if map.get(place).is_some_and(|x| x == "ptr") {
        return true;
    }
    let base = strip_version(place);
    match local_types.get(&base) {
        Some(ty) => cast_spill_ty(ty).is_none() && ty.trim() != "()",
        None => false,
    }
}

fn count_operand(map: &mut BTreeMap<String, usize>, op: &Operand) {
    if let Operand::Move { place , .. } | Operand::Copy { place , .. } | Operand::Conflict { place, .. } = op {
        *map.entry(place.clone()).or_insert(0) += 1;
    }
}

fn count_place(map: &mut BTreeMap<String, usize>, place: &str) {
    *map.entry(place.to_string()).or_insert(0) += 1;
}

/// Base local of a (possibly versioned) register: `_4_v1` -> `_4`,
/// `_1` -> `_1`. Anything else passes through unchanged.
fn strip_version(reg: &str) -> String {
    if let Some(idx) = reg.rfind("_v") {
        let (base, suffix) = reg.split_at(idx);
        if !suffix[2..].is_empty()
            && suffix[2..].chars().all(|ch| ch.is_ascii_digit())
            && is_plain_local(base)
        {
            return base.to_string();
        }
    }
    reg.to_string()
}

/// True for plain MIR local names (`_N`, digits only after the `_`).
fn is_plain_local(reg: &str) -> bool {
    reg.len() > 1
        && reg.starts_with('_')
        && reg[1..].chars().all(|ch| ch.is_ascii_digit())
}

/// Locals that are DEFINED but never READ anywhere in the function.
///
/// Only consulted for byte-literal materialization: a dead `_2 = &"..."` would
/// otherwise allocate a real payload buffer that nobody frees (the old loud
/// path bound the null marker `0`, which the Referee does not count as a
/// leak — so this used to hide). Dropping the dead definition removes the
/// allocation instead of the symptom. MIR Drop/borrow uses count as reads:
/// this set is only used where no other consumer can exist.
pub fn dead_locals(blocks: &[crate::mir::Block], n_params: usize) -> BTreeSet<String> {
    use crate::mir::{Operand, Rvalue, Stmt, Term};
    let mut defined: BTreeSet<String> = BTreeSet::new();
    let mut read: BTreeSet<String> = BTreeSet::new();
    let mut note = |op: &Operand, defined: &mut BTreeSet<String>, read: &mut BTreeSet<String>| {
        let place = match op {
            Operand::Move { place , .. } | Operand::Copy { place , .. } | Operand::Conflict { place, .. } => {
                place.as_str()
            }
            _ => return,
        };
        if defined.contains(place) {
            read.insert(place.to_string());
        }
    };
    for b in blocks {
        for st in &b.statements {
            match st {
                Stmt::Assign { dest, rvalue, .. } => {
                    match rvalue {
                        Rvalue::Use { op } => note(op, &mut defined, &mut read),
                        Rvalue::Repeat { op, .. } => note(op, &mut defined, &mut read),
                        Rvalue::UnOp { operand, .. } | Rvalue::Cast { op: operand, .. } => {
                            note(operand, &mut defined, &mut read)
                        }
                        Rvalue::Call { args, .. } => {
                            for a in args {
                                note(a, &mut defined, &mut read)
                            }
                        }
                        Rvalue::BinOp { left, right, .. } => {
                            note(left, &mut defined, &mut read);
                            note(right, &mut defined, &mut read);
                        }
                        Rvalue::Aggregate { elems, .. } => {
                            for e in elems {
                                note(e, &mut defined, &mut read)
                            }
                        }
                        Rvalue::Ref { place, .. }
                        | Rvalue::Discriminant { place }
                        | Rvalue::RawPtr { place, .. } => {
                            if defined.contains(place.as_str()) {
                                read.insert(place.clone());
                            }
                        }
                        _ => {}
                    }
                    defined.insert(dest.clone());
                }
                Stmt::SetDisc { place, .. } => {
                    if defined.contains(place.as_str()) {
                        read.insert(place.clone());
                    }
                }
                _ => {}
            }
        }
        match &b.terminator {
            Term::Call { args, .. } => {
                for a in args {
                    note(a, &mut defined, &mut read)
                }
            }
            Term::Drop { place, .. } => {
                if defined.contains(place.as_str()) {
                    read.insert(place.clone());
                }
            }
            Term::SwitchInt { discr, .. } => note(discr, &mut defined, &mut read),
            Term::Assert { cond, .. } => note(cond, &mut defined, &mut read),
            Term::InlineAsm { ins, .. } => {
                for a in ins {
                    note(a, &mut defined, &mut read)
                }
            }
            _ => {}
        }
    }
    // Params are always live (the header binds them).
    for i in 1..=n_params {
        defined.remove(&format!("_{}", i));
    }
    defined.retain(|d| !read.contains(d));
    defined
}

/// Slot register name for a spilled base/dest.
pub fn spill_slot(reg: &str) -> String {
    format!("{}_spill", reg)
}

/// Append spill-slot setup lines for this block's synthetic bases
/// (`_agg_{bid}`, `_rep_{bid}`) when the spill map contains them.
/// Appended at the end (always after the base alloc): order-safe.
pub fn spill_slot_lines(lines: &mut Vec<String>, bid: &str, spill: &SpillMap) {
    for base in [format!("_agg_{}", bid), format!("_rep_{}", bid)] {
        if let Some(ty) = spill.get(&base) {
            let slot = spill_slot(&base);
            lines.push(format!("{} = alloc 8", slot));
            lines.push(format!("store {}+0, {} as {}", slot, base, ty));
        }
    }
}

#[cfg(test)]
mod spill_tests {
    use super::*;

    #[test]
    fn call_spill_ty_maps() {
        assert_eq!(call_spill_ty("ptr"), Some("ptr".to_string()));
        assert_eq!(call_spill_ty("u64"), Some("u64".to_string()));
        assert_eq!(call_spill_ty("bool"), Some("u8".to_string()));
        assert_eq!(call_spill_ty("usize"), Some("u64".to_string()));
        assert_eq!(call_spill_ty("?"), None);
    }

    #[test]
    fn synth_base_spills_as_ptr() {
        use crate::mir::{Block, Rvalue, Term};
        let use_agg = Stmt::Assign {
            dest: "_3".to_string(),
            dest_place: Some("_3".to_string()),
            rvalue: Rvalue::Use { op: Operand::Copy { place: "_agg_bb0".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
        };
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![use_agg],
            terminator: Term::Return { ret: None },
        }];
        let m = build_spill(&blocks, &BTreeMap::new());
        assert_eq!(m.get("_agg_bb0"), Some(&"ptr".to_string()));
    }

    #[test]
    fn versioned_i32_with_plain_copies_spills() {
        // 22: _1_v1 = Move _3 (Use arm) with three plain Copy uses
        // must spill as i32 via the driver-typed loop. Versioned
        // defs carry a STALE dest_place (the pre-versioning name),
        // and a place store through the version must not break
        // single-def status either.
        use crate::mir::{Block, Rvalue, Term};
        let mut lt = BTreeMap::new();
        lt.insert("_1".to_string(), "i32".to_string());
        let blocks = vec![
            Block {
                id: "bb0".to_string(),
                statements: vec![
                    Stmt::Assign {
                        dest: "_1_v0".to_string(),
                        dest_place: Some("_1_v0".to_string()),
                        rvalue: Rvalue::Use { op: Operand::Const { value: "Val(Scalar(0x00000000), i32)".to_string(), str_bytes: None, str_len: None } },
                    },
                    Stmt::Assign {
                        dest: "_3".to_string(),
                        dest_place: Some("_3".to_string()),
                        rvalue: Rvalue::Use { op: Operand::Const { value: "Val(Scalar(0x00000001), i32)".to_string(), str_bytes: None, str_len: None } },
                    },
                ],
                terminator: Term::Goto { target: "bb2".to_string() },
            },
            Block {
                id: "bb2".to_string(),
                statements: vec![
                    Stmt::Assign {
                        dest: "_1_v1".to_string(),
                        dest_place: Some("_1".to_string()),
                        rvalue: Rvalue::Use { op: Operand::Move { place: "_3".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                    },
                    Stmt::Assign {
                        dest: "_6".to_string(),
                        dest_place: Some("_6".to_string()),
                        rvalue: Rvalue::Use { op: Operand::Copy { place: "_1_v1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                    },
                ],
                terminator: Term::Goto { target: "bb5".to_string() },
            },
            Block {
                id: "bb5".to_string(),
                statements: vec![
                    Stmt::Assign {
                        dest: "_12".to_string(),
                        dest_place: Some("_12".to_string()),
                        rvalue: Rvalue::Use { op: Operand::Copy { place: "_1_v1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                    },
                    // A place store through _1_v1 is not a def.
                    Stmt::Assign {
                        dest: "_1_v1".to_string(),
                        dest_place: Some("(*_1_v1)".to_string()),
                        rvalue: Rvalue::Use { op: Operand::Move { place: "_9".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                    },
                ],
                terminator: Term::Goto { target: "bb7".to_string() },
            },
            Block {
                id: "bb7".to_string(),
                statements: vec![Stmt::Assign {
                    dest: "_13".to_string(),
                    dest_place: Some("_13".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1_v1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                }],
                terminator: Term::Return { ret: None },
            },
        ];
        let m = build_spill(&blocks, &lt);
        assert_eq!(m.get("_1_v1"), Some(&"i32".to_string()));
    }

    #[test]
    fn no_copies_no_spill() {
        use crate::mir::{Block, Rvalue, Term};
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![],
            terminator: Term::Return { ret: None },
        }];
        assert!(build_spill(&blocks, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn aggregate_root_spills_when_field_copied_twice() {
        // uam-tail: _1 is a tuple aggregate defined by a multi-elem
        // Aggregate; two plain-assign Copy uses are field reads in
        // MIR (driver collapses the subplace to the root), so the
        // root must spill as ptr instead of moving on first use.
        use crate::mir::{Block, Rvalue, Term};
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                Stmt::Assign {
                    dest: "_1".to_string(),
                    dest_place: Some("_1".to_string()),
                    rvalue: Rvalue::Aggregate {
                        elems: vec![
                            Operand::Const { value: "Val(Scalar(0x00000002), i32)".to_string(), str_bytes: None, str_len: None },
                            Operand::Const { value: "Val(Scalar(0x00000005), i32)".to_string(), str_bytes: None, str_len: None },
                        ],
                        layout: Some(crate::mir::AdtLayout { size: 8, offsets: vec![0, 4] }),
                    },
                },
                Stmt::Assign {
                    dest: "_2".to_string(),
                    dest_place: Some("_2".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::Assign {
                    dest: "_3".to_string(),
                    dest_place: Some("_3".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
            ],
            terminator: Term::Return { ret: None },
        }];
        let mut tys = BTreeMap::new();
        tys.insert("_1".to_string(), "(i32, i32)".to_string());
        let m = build_spill(&blocks, &tys);
        assert_eq!(m.get("_1"), Some(&"ptr".to_string()));
    }

    #[test]
    fn versioned_scalar_field_def_does_not_spill() {
        // uam-tail guard: _1_v1 is bound from an i32 temp, so its SA
        // value is scalar even though its base local _1 is an
        // aggregate type; the def gate must keep it off the spill map.
        use crate::mir::{Block, Rvalue, Term};
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                Stmt::Assign {
                    dest: "_1_v1".to_string(),
                    dest_place: Some("_1_v1".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Move { place: "_4".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::Assign {
                    dest: "_2".to_string(),
                    dest_place: Some("_2".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1_v1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::Assign {
                    dest: "_3".to_string(),
                    dest_place: Some("_3".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1_v1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
            ],
            terminator: Term::Return { ret: None },
        }];
        let mut tys = BTreeMap::new();
        tys.insert("_1".to_string(), "Row".to_string());
        tys.insert("_4".to_string(), "i32".to_string());
        let m = build_spill(&blocks, &tys);
        assert!(!m.contains_key("_1_v1"));
    }

    fn cast_copy(dest: &str, place: &str, ty: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: Some(dest.to_string()),
            rvalue: Rvalue::Cast {
                op: Box::new(Operand::Copy { place: place.to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
                ty: ty.to_string(),
                castkind: Some("PtrToPtr".to_string()),
                src_ty: Some("*".to_string()),
            },
        }
    }

    fn borrow_ref(dest: &str, place: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: Some(dest.to_string()),
            rvalue: Rvalue::Ref {
                place: place.to_string(),
                mut_: false,
                via: None,
                zst: false,
            off: 0, off2: 0, deref: false, deref_first: false, proj_loud: false , pty: None , deref_last: false },
        }
    }

    #[test]
    fn cast_spill_ty_maps() {
        assert_eq!(cast_spill_ty("*const std::vec::Vec<i32>"), Some("ptr".to_string()));
        assert_eq!(cast_spill_ty("usize"), Some("u64".to_string()));
        assert_eq!(cast_spill_ty("bool"), Some("u8".to_string()));
        assert_eq!(cast_spill_ty("std::string::String"), None);
        assert_eq!(cast_spill_ty("i128"), None, "128-bit has no SA slot type");
        assert_eq!(
            cast_spill_ty("std::ptr::NonNull<std::task::Context<'{erased}>>"),
            Some("ptr".to_string()),
            "NonNull is a transparent pointer wrapper"
        );
        assert_eq!(
            cast_spill_ty("std::ptr::NonNull<std::task::C"),
            Some("ptr".to_string()),
            "driver-truncated NonNull spelling still maps to ptr"
        );
    }

    #[test]
    fn cast_dest_spills_when_copy_used() {
        use crate::mir::{Block, Rvalue, Term};
        // f_box shape: _7 = cast(copy _6), _12 = cast(copy _6) — pointer-copy
        // chain: the second plain assign would trap UseAfterMove.
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                borrow_ref("_6", "_1"),
                cast_copy("_7", "_6", "*const ()"),
                cast_copy("_12", "_6", "*const ()"),
            ],
            terminator: Term::Return { ret: None },
        }];
        let m = build_spill(&blocks, &BTreeMap::new());
        assert_eq!(m.get("_6"), Some(&"ptr".to_string()));
        assert!(!m.contains_key("_7"), "single use needs no slot");
    }

    #[test]
    fn nonnull_wrapper_spills_when_consumed() {
        use crate::mir::{Block, Rvalue, Term};
        // 09 shape: _25 = cast Transmute to NonNull<Context>, _24 wraps it
        // in a single-elem aggregate, and bb19 consumes _24 with a Move.
        // The cast itself has no Copy use, so only propagation gives _24
        // a slot; without it the Move consumes _24_v0 on one path only
        // (PhiStateConflict at the join).
        let blocks = vec![
            Block {
                id: "bb0".to_string(),
                statements: vec![
                    Stmt::Assign {
                        dest: "_25".to_string(),
                        dest_place: Some("_25".to_string()),
                        rvalue: Rvalue::Cast {
                            op: Box::new(Operand::Move { place: "_2".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
                            ty: "std::ptr::NonNull<std::task::Context<'{erased}>>".to_string(),
                            castkind: Some("Transmute".to_string()),
                            src_ty: Some("&".to_string()),
                        },
                    },
                    Stmt::Assign {
                        dest: "_24".to_string(),
                        dest_place: Some("_24".to_string()),
                        rvalue: Rvalue::Aggregate {
                            elems: vec![Operand::Move { place: "_25".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }],
                            layout: None,
                        },
                    },
                ],
                terminator: Term::Goto { target: "bb1".to_string() },
            },
            Block {
                id: "bb1".to_string(),
                statements: vec![Stmt::Assign {
                    dest: "_12".to_string(),
                    dest_place: Some("_12".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Move { place: "_24".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                }],
                terminator: Term::Return { ret: None },
            },
        ];
        let m = build_spill(&blocks, &BTreeMap::new());
        assert_eq!(m.get("_24"), Some(&"ptr".to_string()));
        assert!(!m.contains_key("_25"), "cast with no Copy use gets no slot itself");
        // Rendered shape: aggregate dest gets its slot setup.
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = crate::render::render_rvalue(
            &Rvalue::Aggregate {
                elems: vec![Operand::Move { place: "_25".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }],
                layout: None,
            },
            "_24", Some("_24"), &mut unsup, "bb0", &mut idx,
            &std::collections::HashMap::new(), &m, &std::collections::BTreeMap::new(),
            &std::collections::BTreeMap::new(),
        );
        assert!(unsup.is_empty());
        assert!(line.contains("_24_spill = alloc 8"), "slot setup:
{}", line);
        assert!(line.contains("store _24_spill+0, _24 as ptr"), "slot store:
{}", line);
    }

    #[test]
    fn borrow_dest_spills_when_consumed() {
        use crate::mir::{Block, Rvalue, Term};
        // main shape: _78 = &_79; _77 = Aggregate([Move(_78)]) — the later
        // `!_78` (borrow-end) must still find the reg bound.
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                borrow_ref("_78", "_79"),
                Stmt::Assign {
                    dest: "_77".to_string(),
                    dest_place: Some("_77".to_string()),
                    rvalue: Rvalue::Aggregate {
                        elems: vec![Operand::Move { place: "_78".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }],
                        layout: None,
                    },
                },
            ],
            terminator: Term::Return { ret: None },
        }];
        assert_eq!(build_spill(&blocks, &BTreeMap::new()).get("_78"), Some(&"ptr".to_string()));
    }

    #[test]
    fn unused_borrow_dest_does_not_spill() {
        use crate::mir::{Block, Term};
        // Only call-arg uses (non-consuming) -> no slot needed.
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![borrow_ref("_5", "_6")],
            terminator: Term::Call {
                func: "f".to_string(),
                func_raw: None,
                args: vec![Operand::Move { place: "_5".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }],
                dest: Some("_7".to_string()),
                target: Some("bb1".to_string()),
                sig: None,
            },
        }];
        assert!(!build_spill(&blocks, &BTreeMap::new()).contains_key("_5"));
    }

    #[test]
    fn byte_const_dest_spills_when_copied() {
        use crate::mir::{Block, Rvalue, Term};
        // b64 shape: `_1 = <&[u8; 3] payload>` then two plain-assign copies.
        // Without a slot the second copy is a UseAfterMove.
        let bytes = vec![77u64, 97, 110];
        let konst = Operand::Const {
            value: "Val(Scalar(alloc1), &'{erased} [u8; 3_usize])".to_string(),
            str_bytes: Some(bytes),
            str_len: Some(3),
        };
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                Stmt::Assign {
                    dest: "_1".to_string(),
                    dest_place: Some("_1".to_string()),
                    rvalue: Rvalue::Use { op: konst.clone() },
                },
                Stmt::Assign {
                    dest: "_4".to_string(),
                    dest_place: Some("_4".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::Assign {
                    dest: "_11".to_string(),
                    dest_place: Some("_11".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
            ],
            terminator: Term::Return { ret: None },
        }];
        let m = build_spill(&blocks, &BTreeMap::new());
        assert_eq!(m.get("_1"), Some(&"ptr".to_string()));
        // Rendered shape: buffer + slot, copies reload.
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = crate::render::render_rvalue(
            &Rvalue::Use { op: konst },
            "_1", Some("_1"), &mut unsup, "bb0", &mut idx,
            &std::collections::HashMap::new(), &m, &std::collections::BTreeMap::new(),
            &std::collections::BTreeMap::new(),
        );
        assert!(unsup.is_empty());
        assert_eq!(
            line,
            "_str_bb0_0 = alloc 3\nstore _str_bb0_0+0, 77 as u8\nstore _str_bb0_0+1, 97 as u8\n\
             store _str_bb0_0+2, 110 as u8\n_1 = _str_bb0_0\n_1_spill = alloc 8\nstore _1_spill+0, _1 as ptr"
        );
    }

    #[test]
    fn fat_byte_const_dest_spills_as_pair_address() {
        use crate::mir::{Block, Rvalue, Term};
        // A `&str` pointee materializes in value position as the (ptr,len)
        // pair's address (see layout::plan_str_pair): a thin `ptr`, so a
        // copied dest claims a slot exactly like a byte-literal dest.
        // (Previously value position could not represent it at all, so the
        // dest stayed loud with no slot; the pair planner changed that.)
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                Stmt::Assign {
                    dest: "_1".to_string(),
                    dest_place: Some("_1".to_string()),
                    rvalue: Rvalue::Use {
                        op: Operand::Const {
                            value: "Val(Slice { alloc_id: alloc1, meta: 3 }, &'{erased} str)".to_string(),
                            str_bytes: Some(vec![104, 105, 33]),
                            str_len: Some(3),
                        },
                    },
                },
                Stmt::Assign {
                    dest: "_4".to_string(),
                    dest_place: Some("_4".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
            ],
            terminator: Term::Return { ret: None },
        }];
        assert_eq!(build_spill(&blocks, &BTreeMap::new()).get("_1"), Some(&"ptr".to_string()));
    }

    fn static_addr_const_def(dest: &str, value: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: Some(dest.to_string()),
            rvalue: Rvalue::Use {
                op: Operand::Const {
                    value: value.to_string(),
                    str_bytes: None,
                    str_len: None,
                },
            },
        }
    }

    fn plain_copy(dest: &str, place: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: Some(dest.to_string()),
            rvalue: Rvalue::Use { op: Operand::Copy { place: place.to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
        }
    }

    #[test]
    fn static_addr_const_dest_spills_when_copied() {
        use crate::mir::{Block, Term};
        // 118 shape: `_1 = Val(Scalar(alloc1), *mut i32)` then two
        // plain-assign copies (via casts in real MIR; Use copies here
        // exercise the same copy_use_targets gate).
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                static_addr_const_def("_1", "Val(Scalar(alloc1), *mut i32)"),
                plain_copy("_35", "_1"),
                plain_copy("_40", "_1"),
            ],
            terminator: Term::Return { ret: None },
        }];
        assert_eq!(build_spill(&blocks, &BTreeMap::new()).get("_1"), Some(&"ptr".to_string()));
    }

    #[test]
    fn static_addr_const_dest_spills_on_single_copy_use() {
        use crate::mir::{Block, Term};
        // Same gate as the call/cast/borrow branches: presence of a
        // Copy-use (not 2+) claims the slot; the reload is harmless.
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                static_addr_const_def("_1", "Val(Scalar(alloc1), *mut i32)"),
                plain_copy("_35", "_1"),
            ],
            terminator: Term::Return { ret: None },
        }];
        assert_eq!(build_spill(&blocks, &BTreeMap::new()).get("_1"), Some(&"ptr".to_string()));
    }

    #[test]
    fn static_addr_const_dest_no_spill_when_unused() {
        use crate::mir::{Block, Term};
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![static_addr_const_def(
                "_1",
                "Val(Scalar(alloc1), *mut i32)",
            )],
            terminator: Term::Return { ret: None },
        }];
        assert!(build_spill(&blocks, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn fat_ref_alloc_const_does_not_spill() {
        use crate::mir::{Block, Term};
        // `&str` alloc consts without resolved bytes are fat (ptr,len):
        // const_elem_ty stays untyped, so no thin-ptr slot is claimed.
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                static_addr_const_def("_1", "Val(Scalar(alloc1), &'{erased} str)"),
                plain_copy("_4", "_1"),
                plain_copy("_11", "_1"),
            ],
            terminator: Term::Return { ret: None },
        }];
        assert!(!build_spill(&blocks, &BTreeMap::new()).contains_key("_1"));
    }

    #[test]
    fn setdisc_place_spills_when_copy_used() {
        use crate::mir::{Block, Rvalue, Term};
        // 09_async_await shape: _21 = Copy _1, _3 = Copy _21, SetDisc _21.
        // Without a slot the SetDisc store reads a register the plain
        // assign already moved (UseAfterMove).
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                Stmt::Assign {
                    dest: "_21".to_string(),
                    dest_place: Some("_21".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::Assign {
                    dest: "_3".to_string(),
                    dest_place: Some("_3".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_21".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::SetDisc { place: "_21".to_string(), variant: 1, place_full: None },
            ],
            terminator: Term::Return { ret: None },
        }];
        assert_eq!(build_spill(&blocks, &BTreeMap::new()).get("_21"), Some(&"ptr".to_string()));
    }

    #[test]
    fn setdisc_place_without_plain_use_does_not_spill() {
        use crate::mir::{Block, Rvalue, Term};
        // 75_async_bridge shape: _4 = Copy _1, Discriminant _4, SetDisc _4.
        // Nothing moves _4, so the store is already legal: no slot.
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                Stmt::Assign {
                    dest: "_4".to_string(),
                    dest_place: Some("_4".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::Assign {
                    dest: "_3".to_string(),
                    dest_place: Some("_3".to_string()),
                    rvalue: Rvalue::Discriminant { place: "_4".to_string() },
                },
                Stmt::SetDisc { place: "_4".to_string(), variant: 1, place_full: None },
            ],
            terminator: Term::Return { ret: None },
        }];
        assert!(!build_spill(&blocks, &BTreeMap::new()).contains_key("_4"));
    }

    #[test]
    fn setdisc_place_multi_def_does_not_spill() {
        use crate::mir::{Block, Rvalue, Term};
        // 09 _23 shape (pre-versioning): two Assign defs, so the
        // single-def gate excludes it; versioning owns multi-def locals.
        let def = |v: u32| Stmt::Assign {
            dest: "_23".to_string(),
            dest_place: Some("_23".to_string()),
            rvalue: Rvalue::Use { op: Operand::Copy { place: format!("_{}", v) , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
        };
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                def(1),
                def(2),
                Stmt::Assign {
                    dest: "_3".to_string(),
                    dest_place: Some("_3".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_23".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::SetDisc { place: "_23".to_string(), variant: 1, place_full: None },
            ],
            terminator: Term::Return { ret: None },
        }];
        assert!(!build_spill(&blocks, &BTreeMap::new()).contains_key("_23"));
    }

    #[test]
    fn setdisc_place_use_def_sets_up_slot() {
        use crate::mir::{Block, Rvalue, Term};
        // A spilled Use def must set up its own slot (other arms do this
        // inline); otherwise Copy-uses would reload from an empty slot.
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                Stmt::Assign {
                    dest: "_21".to_string(),
                    dest_place: Some("_21".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::Assign {
                    dest: "_3".to_string(),
                    dest_place: Some("_3".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_21".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::SetDisc { place: "_21".to_string(), variant: 1, place_full: None },
            ],
            terminator: Term::Return { ret: None },
        }];
        let m = build_spill(&blocks, &BTreeMap::new());
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = crate::render::render_rvalue(
            &Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
            "_21", Some("_21"), &mut unsup, "bb0", &mut idx,
            &std::collections::HashMap::new(), &m, &std::collections::BTreeMap::new(),
            &std::collections::BTreeMap::new(),
        );
        assert!(unsup.is_empty());
        assert_eq!(line, "_21 = _1\n_21_spill = alloc 8\nstore _21_spill+0, _21 as ptr");
    }

    #[test]
    fn zst_borrow_never_spills() {
        use crate::mir::{Block, Rvalue, Term};
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                Stmt::Assign {
                    dest: "_6".to_string(),
                    dest_place: Some("_6".to_string()),
                    rvalue: Rvalue::Ref {
                        place: "_1".to_string(),
                        mut_: false,
                        via: None,
                        zst: true,
                    off: 0, off2: 0, deref: false, deref_first: false, proj_loud: false , pty: None , deref_last: false },
                },
                cast_copy("_7", "_6", "usize"),
            ],
            terminator: Term::Return { ret: None },
        }];
        assert!(!build_spill(&blocks, &BTreeMap::new()).contains_key("_6"));
    }

    #[test]
    fn driver_local_type_spills_scalar_copy() {
        // phi-11: driver local types let scalar Copy sources spill
        // exactly (03: _1/_2 params Copy-used on the arms).
        use crate::mir::{Block, Rvalue, Term};
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![
                Stmt::Assign {
                    dest: "_2".to_string(), dest_place: None,
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
                Stmt::Assign {
                    dest: "_3".to_string(), dest_place: None,
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                },
            ],
            terminator: Term::Return { ret: None },
        }];
        let mut lt = BTreeMap::new();
        lt.insert("_1".to_string(), "i32".to_string());
        assert_eq!(build_spill(&blocks, &lt).get("_1"), Some(&"i32".to_string()));
    }

    #[test]
    fn driver_local_type_adt_no_spill() {
        // ADTs map to no slot type (105/39: Option<i32> Copy sources
        // stay on the old paths; the join fix handles them instead).
        use crate::mir::{Block, Rvalue, Term};
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![Stmt::Assign {
                dest: "_2".to_string(), dest_place: None,
                rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
            }],
            terminator: Term::Return { ret: None },
        }];
        let mut lt = BTreeMap::new();
        lt.insert("_1".to_string(), "Option<i32>".to_string());
        assert!(!build_spill(&blocks, &lt).contains_key("_1"));
    }

    #[test]
    fn driver_local_type_versioned_base() {
        // 103-shape: a versioned Copy use resolves its type via the
        // base local (_4_v1 -> _4).
        use crate::mir::{Block, Rvalue, Term};
        let blocks = vec![
            Block {
                id: "bb0".to_string(),
                statements: vec![Stmt::Assign {
                    dest: "_4_v1".to_string(), dest_place: None,
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_0".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                }],
                terminator: Term::Goto { target: "bb1".to_string() },
            },
            Block {
                id: "bb1".to_string(),
                statements: vec![Stmt::Assign {
                    dest: "_7".to_string(), dest_place: None,
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_4_v1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                }],
                terminator: Term::Goto { target: "bb2".to_string() },
            },
            Block {
                id: "bb2".to_string(),
                statements: vec![Stmt::Assign {
                    dest: "_9".to_string(), dest_place: None,
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_4_v1".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false } },
                }],
                terminator: Term::Return { ret: None },
            },
        ];
        let mut lt = BTreeMap::new();
        lt.insert("_4".to_string(), "i32".to_string());
        assert_eq!(build_spill(&blocks, &lt).get("_4_v1"), Some(&"i32".to_string()));
    }
}
