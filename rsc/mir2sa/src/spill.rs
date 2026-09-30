//! mir2sa::spill — reload slots for multi-use values.
//!
//! SA `=` moves (single-assignment): `_X = Copy _Y` traps UseAfterMove when
//! `_Y` is read again later. Shared reads inside call args / BinOp operands /
//! stores / eq are fine (non-consuming) — only plain-assign copies need help.
//!
//! For values whose TYPE IS KNOWN, spill to a slot once and reload per copy:
//! - synthetic buffers (`_agg_*`, `_rep_*`): always `ptr` (we generate them);
//! - call dests with resolvable sig ret: mapped store-ty (u64/i64/u32/...).
//! Moves stay direct (single exact transfer); only Copy-uses reload, so the
//! transform is order-independent (loads never touch the source reg).
//! Slots are always 8 bytes (all store-tys fit; slack is harmless).
//! Everything else (computed scalars, params, borrows) stays loud — the
//! general program needs driver locals widths (spill) and SSA versioning.

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
                Operand::Copy { place } => vec![place.clone()],
                _ => vec![],
            },
            Rvalue::Aggregate { elems, .. } if elems.len() == 1 => match &elems[0] {
                Operand::Copy { place } => vec![place.clone()],
                _ => vec![],
            },
            Rvalue::Cast { op, .. } => match op.as_ref() {
                Operand::Copy { place } => vec![place.clone()],
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
                Operand::Move { place } => vec![place.clone()],
                _ => vec![],
            },
            Rvalue::Aggregate { elems, .. } if elems.len() == 1 => match &elems[0] {
                Operand::Move { place } => vec![place.clone()],
                _ => vec![],
            },
            Rvalue::Cast { op, .. } => match op.as_ref() {
                Operand::Move { place } => vec![place.clone()],
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
pub fn build_spill(blocks: &[crate::mir::Block]) -> SpillMap {
    // Copy-use targets across the whole function.
    let mut copies: BTreeSet<String> = BTreeSet::new();
    // Consuming (Move) uses: these need a slot for the SOURCE to survive.
    let mut consuming: BTreeSet<String> = BTreeSet::new();
    for b in blocks {
        for st in &b.statements {
            copies.extend(copy_use_targets(st));
            consuming.extend(consuming_use_targets(st));
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
            if let Stmt::Assign { dest, .. } = st {
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
            if let Stmt::Assign { dest, rvalue: Rvalue::Cast { ty, castkind, src_ty, .. }, .. } = st {
                if copies.contains(dest) && single(dest) && !map.contains_key(dest) {
                    let dst = crate::asm::cast_dst_short(ty);
                    let lowers = match (castkind.as_deref(), src_ty.as_deref()) {
                        (Some(k), Some(s)) => {
                            crate::asm::lower_cast(k, s, &dst).is_some()
                        }
                        _ => false,
                    };
                    if lowers {
                        if let Some(t) = cast_spill_ty(ty) {
                            map.insert(dest.clone(), t);
                        }
                    }
                }
            }
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
    // Byte-literal const dests (`_x = <&[u8; K] payload>`): the value is a
    // buffer address (known `ptr`). MIR Copy-uses of a Copy-typed local lower
    // to plain assigns, which consume the reg — a later `Copy` (or the
    // borrow-end release) would then read a reg the Referee dropped.
    for b in blocks {
        for st in &b.statements {
            if let Stmt::Assign { dest, rvalue: Rvalue::Use { op: Operand::Const { value, str_bytes, str_len } }, .. } = st {
                let inlineable =
                    crate::layout::plan_const_bytes(dest, "bb", 0, value, str_bytes.as_ref(), *str_len)
                        .is_some();
                let used = copies.contains(dest) || consuming.contains(dest);
                if inlineable && used && single(dest) && !map.contains_key(dest) {
                    map.insert(dest.clone(), "ptr".to_string());
                }
            }
        }
    }
    map
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
            rvalue: Rvalue::Use { op: Operand::Copy { place: "_agg_bb0".to_string() } },
        };
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![use_agg],
            terminator: Term::Return { ret: None },
        }];
        let m = build_spill(&blocks);
        assert_eq!(m.get("_agg_bb0"), Some(&"ptr".to_string()));
    }

    #[test]
    fn no_copies_no_spill() {
        use crate::mir::{Block, Rvalue, Term};
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![],
            terminator: Term::Return { ret: None },
        }];
        assert!(build_spill(&blocks).is_empty());
    }

    fn cast_copy(dest: &str, place: &str, ty: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: Some(dest.to_string()),
            rvalue: Rvalue::Cast {
                op: Box::new(Operand::Copy { place: place.to_string() }),
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
            },
        }
    }

    #[test]
    fn cast_spill_ty_maps() {
        assert_eq!(cast_spill_ty("*const std::vec::Vec<i32>"), Some("ptr".to_string()));
        assert_eq!(cast_spill_ty("usize"), Some("u64".to_string()));
        assert_eq!(cast_spill_ty("bool"), Some("u8".to_string()));
        assert_eq!(cast_spill_ty("std::string::String"), None);
        assert_eq!(cast_spill_ty("i128"), None, "128-bit has no SA slot type");
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
        let m = build_spill(&blocks);
        assert_eq!(m.get("_6"), Some(&"ptr".to_string()));
        assert!(!m.contains_key("_7"), "single use needs no slot");
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
                        elems: vec![Operand::Move { place: "_78".to_string() }],
                        layout: None,
                    },
                },
            ],
            terminator: Term::Return { ret: None },
        }];
        assert_eq!(build_spill(&blocks).get("_78"), Some(&"ptr".to_string()));
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
                args: vec![Operand::Move { place: "_5".to_string() }],
                dest: Some("_7".to_string()),
                target: Some("bb1".to_string()),
                sig: None,
            },
        }];
        assert!(!build_spill(&blocks).contains_key("_5"));
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
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() } },
                },
                Stmt::Assign {
                    dest: "_11".to_string(),
                    dest_place: Some("_11".to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() } },
                },
            ],
            terminator: Term::Return { ret: None },
        }];
        let m = build_spill(&blocks);
        assert_eq!(m.get("_1"), Some(&"ptr".to_string()));
        // Rendered shape: buffer + slot, copies reload.
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = crate::render::render_rvalue(
            &Rvalue::Use { op: konst },
            "_1", Some("_1"), &mut unsup, "bb0", &mut idx,
            &std::collections::HashMap::new(), &m,
        );
        assert!(unsup.is_empty());
        assert_eq!(
            line,
            "_str_bb0_0 = alloc 3\nstore _str_bb0_0+0, 77 as u8\nstore _str_bb0_0+1, 97 as u8\n\
             store _str_bb0_0+2, 110 as u8\n_1 = _str_bb0_0\n_1_spill = alloc 8\nstore _1_spill+0, _1 as ptr"
        );
    }

    #[test]
    fn fat_byte_const_dest_does_not_spill() {
        use crate::mir::{Block, Rvalue, Term};
        // A `&str` pointee is a fat pointer: value position cannot represent
        // it, so the dest stays loud and gets no slot.
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
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_1".to_string() } },
                },
            ],
            terminator: Term::Return { ret: None },
        }];
        assert!(!build_spill(&blocks).contains_key("_1"));
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
                    },
                },
                cast_copy("_7", "_6", "usize"),
            ],
            terminator: Term::Return { ret: None },
        }];
        assert!(!build_spill(&blocks).contains_key("_6"));
    }
}
