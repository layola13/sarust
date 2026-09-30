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

/// Copy-use positions that need reload (plain-assign moves of shared reads):
/// Assign-Use RHS and single-elem aggregate elems. All other Copy positions
/// render shared reads inline (non-consuming) and need nothing.
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
    for b in blocks {
        for st in &b.statements {
            copies.extend(copy_use_targets(st));
        }
    }
    if copies.is_empty() {
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
            terminator: Term::Return,
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
            terminator: Term::Return,
        }];
        assert!(build_spill(&blocks).is_empty());
    }
}
