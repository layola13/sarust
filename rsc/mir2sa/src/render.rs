//! mir2sa::render.
use crate::asm::{cast_dst_short, lower_cast, CastLower};
use crate::const_util::{repeat_plan, sa_scalar_ty};
use crate::layout::{lower_adt_init, lower_array_init, tls_key};
use crate::mir::*;
use crate::render_util::*;

/// Operand text for a spilled source: a `load` from its slot. None when the
/// operand is not a spilled register (constants, spills absent).
pub fn spill_reg(op: &Operand, spill: &crate::spill::SpillMap) -> Option<String> {
    let place = match op {
        Operand::Move { place } | Operand::Copy { place } => place,
        _ => return None,
    };
    let t = spill.get(place)?;
    Some(format!("load {}+0 as {}", crate::spill::spill_slot(place), t))
}

/// Aggregate-copy layouts (phi-11, 308): defined local -> (total alloc
/// bytes, per-elem offsets) for multi-elem Aggregate defs whose elems
/// are all Move/Copy. Such defs lower through layout::lower_adt_init,
/// which stores every Move/Copy elem as a u64 slot; a later Copy of
/// the aggregate must duplicate those slots instead of moving the base
/// pointer (a second Copy would trap UseAfterMove). Offsets follow
/// lower_adt_init exactly: the driver layout when its arity matches,
/// else 8-byte strides; total is the max of the 8-byte footprint and
/// the driver size. Defs without a dest_place go loud (placeholder),
/// so they are not in the map.
pub type AggCopyMap = std::collections::BTreeMap<String, (usize, Vec<usize>)>;

pub fn build_agg_copy(blocks: &[Block]) -> AggCopyMap {
    let mut map = AggCopyMap::new();
    for b in blocks {
        for st in &b.statements {
            let Stmt::Assign { dest, dest_place: Some(_), rvalue: Rvalue::Aggregate { elems, layout } } = st else {
                continue;
            };
            if elems.len() < 2 {
                continue;
            }
            let all_movable = elems.iter().all(|e| {
                matches!(e, Operand::Move { .. } | Operand::Copy { .. })
            });
            if !all_movable {
                continue;
            }
            let n = elems.len();
            let (total, offsets) = match layout {
                Some(l) if l.offsets.len() == n && l.size > 0 => {
                    (std::cmp::max(8 * n, l.size as usize), l.offsets.iter().map(|o| *o as usize).collect())
                }
                _ => (8 * n, (0..n).map(|i| 8 * i).collect()),
            };
            map.insert(dest.clone(), (total, offsets));
        }
    }
    map
}

pub fn render_rvalue(
    rv: &Rvalue,
    dest: &str,
    dest_place: Option<&str>,
    unsup: &mut Vec<String>,
    bid: &str,
    mv_idx: &mut usize,
    cmap: &std::collections::HashMap<String, String>,
    spill: &crate::spill::SpillMap,
    agg_copy: &AggCopyMap,
) -> String {
    match rv {
        Rvalue::Use { op } => {
            // Spilled shared values reload (never move the slot source).
            if let Operand::Copy { place } = op {
                if let Some(ty) = spill.get(place) {
                    let mut s = format!(
                        "{} = load {}+0 as {}",
                        dest,
                        crate::spill::spill_slot(place),
                        ty
                    );
                    // A spilled dest keeps its own slot too (SetDisc-gated
                    // Use defs land here when source and dest both spill).
                    if let Some(t) = spill.get(dest) {
                        let slot = crate::spill::spill_slot(dest);
                        s.push_str(&format!(
                            "\n{} = alloc 8\nstore {}+0, {} as {}",
                            slot, slot, dest, t
                        ));
                    }
                    return s;
                }
            }
            if let Some(t) = spill_reg(op, spill) {
                return format!("{} = {}", dest, t);
            }
            // Copy of a multi-elem aggregate duplicates its slots (see
            // build_agg_copy): moving the base would consume the source
            // and a second Copy would trap UseAfterMove (308: _9 copied
            // into _17 and _18).
            if let Operand::Copy { place } = op {
                if let Some((total, offsets)) = agg_copy.get(place) {
                    let base = format!("_aggcp_{}_{}", bid, dest);
                    let mut lines = vec![format!("{} = alloc {}", base, total)];
                    for (i, off) in offsets.iter().enumerate() {
                        let tmp = format!("_cp_{}_{}_{}", bid, dest, i);
                        lines.push(format!("{} = load {}+{} as u64", tmp, place, off));
                        lines.push(format!("store {}+{}, {} as u64", base, off, tmp));
                    }
                    lines.push(format!("{} = {}", dest, base));
                    if let Some(t) = spill.get(dest) {
                        let slot = crate::spill::spill_slot(dest);
                        lines.push(format!("{} = alloc 8", slot));
                        lines.push(format!("store {}+0, {} as {}", slot, dest, t));
                    }
                    return lines.join("\n");
                }
            }
            // Const-propagated literals re-materialize instead of moving a
            // shared temp (a second move of the temp would trap UseAfterMove).
            // Sound: literals carry no control or ownership dependence.
            match op {
                Operand::Move { place } | Operand::Copy { place } => {
                    let mut s = if let Some(lit) = cmap.get(place) {
                        format!("{} = {}", dest, lit)
                    } else {
                        format!("{} = {}", dest, render_operand(op))
                    };
                    // Spilled dests (SetDisc-gated Use defs) keep a slot so
                    // later Copy-uses and SetDisc stores reload instead of
                    // moving the original register.
                    if let Some(t) = spill.get(dest) {
                        let slot = crate::spill::spill_slot(dest);
                        s.push_str(&format!(
                            "\n{} = alloc 8\nstore {}+0, {} as {}",
                            slot, slot, dest, t
                        ));
                    }
                    s
                }
                // Byte-literal const in value position: inline the payload
                // and bind its thin address (fat pointees stay loud).
                Operand::Const { value, str_bytes, str_len } => {
                    if let Some(mut lines) = crate::layout::plan_const_bytes(
                        dest, bid, *mv_idx, value, str_bytes.as_ref(), *str_len,
                    ) {
                        *mv_idx += 1;
                        // Spilled dests keep a slot so their Copy-uses reload
                        // instead of moving the buffer address.
                        if let Some(t) = spill.get(dest) {
                            let s = crate::spill::spill_slot(dest);
                            lines.push(format!("{} = alloc 8", s));
                            lines.push(format!("store {}+0, {} as {}", s, dest, t));
                        }
                        return lines.join("\n");
                    }
                    // Plain scalar const fallback: like every other Use
                    // arm, a spilled dest keeps its slot (315: _1_v0 = 2
                    // bound no slot, then a Copy use reloaded from the
                    // unbound slot and trapped UnknownRegister).
                    let mut s = format!("{} = {}", dest, render_operand(op));
                    if let Some(t) = spill.get(dest) {
                        let slot = crate::spill::spill_slot(dest);
                        s.push_str(&format!(
                            "\n{} = alloc 8\nstore {}+0, {} as {}",
                            slot, slot, dest, t
                        ));
                    }
                    s
                }
                // Conflict sentinels never reach here (intercepted loud
                // upstream); the fallback keeps rendering total.
                _ => format!("{} = {}", dest, render_operand(op)),
            }
        }
        Rvalue::Ref { place, mut_, via, zst } => {
            // Zero-sized borrows carry no data: null marker (exact).
            // Trailing `//` is ForbiddenSyntax: provenance goes full-line
            // (the Assign arm indents each line of multi-line returns).
            if *zst {
                return format!("// zero-sized borrow (no storage)\n{} = 0", dest);
            }
            let mut lines = vec![];
            if *mut_ {
                lines.push("// mut-borrow (Phase1: & + Referee)".to_string());
            }
            if let Some(v) = via {
                lines.push(format!("// via {}", flat_comment(v)));
            }
            lines.push(format!("{} = &{}", dest, place));
            // Spilled borrow temps keep a slot: their MIR Copy-uses reload,
            // so the later `drop(b)` still finds `b` bound.
            if let Some(t) = spill.get(dest) {
                let s = crate::spill::spill_slot(dest);
                lines.push(format!("{} = alloc 8", s));
                lines.push(format!("store {}+0, {} as {}", s, dest, t));
            }
            lines.join("\n")
        }
        Rvalue::Call { func, args, sig } => {
            if call_sig_loud(sig, args) {
                unsup.push(format!("{}:{} CallNoSig", bid, dest));
                return format!("// UNSUPPORTED call-sig -> {}: unresolvable callee signature", dest);
            }
            let a: Vec<String> = args.iter().map(render_call_arg).collect();
            let mut s = format!("{} = call @{}({})", dest, sa_ident(func), a.join(", "));
            // Spilled call dests get their slot setup inline.
            if let Some(ty) = spill.get(dest) {
                let slot = crate::spill::spill_slot(dest);
                s.push_str(&format!("\n{} = alloc 8\nstore {}+0, {} as {}", slot, slot, dest, ty));
            }
            s
        }
        Rvalue::BinOp { op, left, right } => {
            // `*WithOverflow` yields a (value, flag) PAIR, which this backend
            // cannot hold in one register. rustc always follows it with an
            // `Assert(Overflow(..))` on the flag, so the value alone is what
            // the program observes: lower to the sa_std checked helper, which
            // returns the exact value and traps on overflow (the assert is
            // folded there, see lower.rs). Panic-on-overflow is Rust's
            // `overflow-checks` behaviour, not an invention.
            if let Some(helper) = checked_arith_helper(op) {
                let (pl, lt) = bind_move_operand(left, bid, mv_idx);
                let (pr, rt) = bind_move_operand(right, bid, mv_idx);
                let mut lines = pl;
                lines.extend(pr);
                lines.push(format!("{} = call @{}({}, {})", dest, helper, lt, rt));
                return lines.join("\n");
            }
            let (pl, lt) = bind_move_operand(left, bid, mv_idx);
            let (pr, rt) = bind_move_operand(right, bid, mv_idx);
            match binop_mnemonic(op) {
                Some(m) => {
                    let mut lines = pl;
                    lines.extend(pr);
                    lines.push(format!("{} = {} {}, {}", dest, m, lt, rt));
                    lines.join("\n")
                }
                None => {
                    unsup.push(format!("{}:{} BinOp-{}", bid, dest, op));
                    format!("// UNSUPPORTED binop -> {}: {}({}, {})", dest, op, flat_comment(&lt), flat_comment(&rt))
                }
            }
        }
        Rvalue::UnOp { op, operand } => {
            // Fat-pointer metadata lives at offset 8 of the (ptr,len) pair —
            // the same layout `slice.sal` uses for string fields — so the meta
            // read is an exact `load p+8` (no mnemonic exists for it).
            if *op == "PtrMetadata" {
                let mut idx = 0usize;
                let (pre, t) = bind_move_operand(operand, bid, &mut idx);
                let mut lines = pre;
                lines.push(format!("{} = load {}+8 as u64", dest, t));
                return lines.join("\n");
            }
            let mut idx = 0usize;
            let (pre, t) = bind_move_operand(operand, bid, &mut idx);
            match unop_mnemonic(op) {
                Some(m) => {
                    let mut lines = pre;
                    lines.push(format!("{} = {} {}", dest, m, t));
                    lines.join("\n")
                }
                None => {
                    unsup.push(format!("{}:{} UnOp-{}", bid, dest, op));
                    format!("// UNSUPPORTED unop -> {}: {}({})", dest, op, flat_comment(&t))
                }
            }
        }
        Rvalue::Cast { op, ty, castkind, src_ty } => {
            let dst = cast_dst_short(ty);
            // Spilled sources reload instead of moving (pointer-copy chains
            // reuse one value 2-3 times, and a borrow temp must stay bound
            // for its own later `!b`; `sa check` traps the plain assign as
            // UseAfterMove / UnknownRegister).
            let reload = spill_reg(op, spill);
            let (mut pre, ot) = match &reload {
                Some(t) => (vec![], t.clone()),
                None => bind_move_operand(op, bid, mv_idx),
            };
            // SA emission type for conversions (`as TY`).
            let emit_ty = if dst == "ptr" {
                "ptr".to_string()
            } else {
                sa_scalar_ty(dst.trim()).to_string()
            };
            // Spilled cast dests get their slot setup here (same shape as the
            // repeat/aggregate synthetic bases).
            let slot = |pre: &mut Vec<String>| {
                if let Some(t) = spill.get(dest) {
                    let s = crate::spill::spill_slot(dest);
                    pre.push(format!("{} = alloc 8", s));
                    pre.push(format!("store {}+0, {} as {}", s, dest, t));
                }
            };
            match (castkind.as_deref(), src_ty.as_deref()) {
                (Some(k), Some(s)) => match lower_cast(k, s, &dst) {
                    Some(CastLower::Copy) => {
                        pre.push(format!("{} = {}", dest, ot));
                        slot(&mut pre);
                        pre.join("\n")
                    }
                    Some(CastLower::Convert(m)) => {
                        // The conversion operand must be a REGISTER: a
                        // reload expression cannot be nested (`zext load ..
                        // as u8 as i32` is UnknownRegister, sla-201 shape),
                        // so bind the reload to a temp first.
                        match &reload {
                            Some(t) => {
                                let tmp = format!("_mv_{}_{}", bid, mv_idx);
                                *mv_idx += 1;
                                pre.push(format!("{} = {}", tmp, t));
                                pre.push(format!("{} = {} {} as {}", dest, m, tmp, emit_ty));
                            }
                            None => pre.push(format!("{} = {} {} as {}", dest, m, ot, emit_ty)),
                        }
                        slot(&mut pre);
                        pre.join("\n")
                    }
                    None => {
                        unsup.push(format!("{}:{} Cast-{}", bid, dest, k.split('(').next().unwrap_or(k)));
                        format!("// UNSUPPORTED cast -> {}: {} as {} (src {})", dest, flat_comment(&ot), flat_comment(ty), flat_comment(s))
                    }
                },
                // Old-schema fixture without kind fields: legacy loud path.
                _ => {
                    unsup.push(format!("{}:{} Cast", bid, dest));
                    format!("// UNSUPPORTED cast -> {}: {} as {}", dest, flat_comment(&ot), flat_comment(ty))
                }
            }
        }
        Rvalue::Discriminant { place } => {
            // Tag lives at offset 0 (mirrors SetDisc `store +0`).
            format!("{} = load {}+0 as i64", dest, place)
        }
        Rvalue::RawPtr { place, .. } => {
            // Thin-pointer reborrow is a plain copy (exact); fat cases stay
            // approximate under the base-local collapse (documented).
            format!("{} = {}", dest, place)
        }
        Rvalue::Repeat { op, len } => {
            match op.as_ref() {
                Operand::Const { value, .. } => match repeat_plan(value, len) {
                    Some((v, total)) => {
                        let mut s = format!(
                            "// repeat [{}; {}]\n_rep_{} = alloc {}\ncall @sa_mem_set(&_rep_{}, {}, {})\n{} = _rep_{}",
                            value, len, bid, total, bid, v, total, dest, bid
                        );
                        // Spilled repeat buffers get their slot setup here
                        // (same shape as aggregate bases).
                        let base = format!("_rep_{}", bid);
                        if let Some(ty) = spill.get(&base) {
                            let slot = crate::spill::spill_slot(&base);
                            s.push_str(&format!("\n{} = alloc 8\nstore {}+0, {} as {}", slot, slot, base, ty));
                        }
                        s
                    }
                    None => {
                        unsup.push(format!("{}:{} Repeat", bid, dest));
                        format!("// UNSUPPORTED repeat -> {}: [{}; {}]", dest, render_operand(op), len)
                    }
                },
                _ => {
                    unsup.push(format!("{}:{} Repeat", bid, dest));
                    format!("// UNSUPPORTED repeat -> {}: [{}; {}] (non-const elem needs loop)", dest, render_operand(op), len)
                }
            }
        }

        Rvalue::ThreadLocal { def } => {
            // Lowered to the shared registry (sci/sa_std/thread_local.sai).
            format!("// thread-local registry slot\n{} = call @sa_thread_local_slot({})", dest, tls_key(def))
        }
        Rvalue::Aggregate { elems, layout } => {
            if elems.is_empty() {
                format!("// zero-elem aggregate (unit/niche; tag via SetDisc when present)\n{} = 0", dest)
            } else if elems.len() == 1 {
                // Spilled shared buffers reload like plain Uses.
                let mut s = if let Some(t) = spill_reg(&elems[0], spill) {
                    format!(
                        "// single-elem aggregate (spill reload)\n{} = {}",
                        dest, t
                    )
                } else {
                    format!("// single-elem aggregate (exact)\n{} = {}", dest, render_operand(&elems[0]))
                };
                // Spilled aggregate dests (type propagated from a
                // pointer-valued element, e.g. a NonNull cast) get their
                // slot setup here, same shape as cast/ref/call dests, so
                // later uses reload instead of moving the register.
                if let Some(t) = spill.get(dest) {
                    let slot = crate::spill::spill_slot(dest);
                    s.push_str(&format!("\n{} = alloc 8\nstore {}+0, {} as {}", slot, slot, dest, t));
                }
                s
            } else if let Some(place) = dest_place {
                // Array fast path first (exact [T; N] all-const, incl. ManuallyDrop backing).
                if let Some(mut lines) = lower_array_init(bid, place, elems) {
                    crate::spill::spill_slot_lines(&mut lines, bid, spill);
                    lines.join("\n")
                } else if let Some(mut lines) = lower_adt_init(bid, dest, place, elems, layout.as_ref()) {
                    crate::spill::spill_slot_lines(&mut lines, bid, spill);
                    lines.join("\n")
                } else {
                    let e: Vec<String> = elems.iter().map(render_operand).collect();
                    unsup.push(format!("{}:{} Aggregate", bid, dest));
                    format!("// UNSUPPORTED multi-elem aggregate -> {} @ {}: {}",
                            dest, place.trim(), e.join(", "))
                }
            } else {
                let e: Vec<String> = elems.iter().map(render_operand).collect();
                unsup.push(format!("{}:{} Aggregate", bid, dest));
                format!("// UNSUPPORTED multi-elem aggregate -> {}: {}", dest, e.join(", "))
            }
        }
        Rvalue::Unsupported { text } => {
            unsup.push(format!("{}:{} Unsupported", bid, dest));
            format!("// UNSUPPORTED rvalue -> {}: {}", dest, text)
        }
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::mir::{Block, Operand, Rvalue, Stmt, Term};

    #[test]
    fn agg_copy_map_and_render() {
        // 308-shape: _9 = Aggregate(Move _10, Move _11) with a rustc
        // layout (size 16, offsets [0, 8]). A later Copy of _9 must
        // duplicate the slots, not move the base pointer.
        let blocks = vec![Block {
            id: "bb2".to_string(),
            statements: vec![Stmt::Assign {
                dest: "_9".to_string(),
                dest_place: Some("_9".to_string()),
                rvalue: Rvalue::Aggregate {
                    elems: vec![
                        Operand::Move { place: "_10".to_string() },
                        Operand::Move { place: "_11".to_string() },
                    ],
                    layout: Some(crate::mir::AdtLayout { size: 16, offsets: vec![0, 8] }),
                },
            }],
            terminator: Term::Goto { target: "bb3".to_string() },
        }];
        let map = build_agg_copy(&blocks);
        assert_eq!(map.get("_9"), Some(&(16usize, vec![0usize, 8usize])));
        let mut unsup = vec![];
        let mut idx = 0usize;
        let line = render_rvalue(
            &Rvalue::Use { op: Operand::Copy { place: "_9".to_string() } },
            "_17", Some("_17"), &mut unsup, "bb2", &mut idx,
            &std::collections::HashMap::new(), &std::collections::BTreeMap::new(), &map,
        );
        assert!(unsup.is_empty());
        assert!(line.contains("_aggcp_bb2__17 = alloc 16"), "alloc:
{}", line);
        assert!(line.contains("load _9+0 as u64"), "slot 0 load:
{}", line);
        assert!(line.contains("load _9+8 as u64"), "slot 8 load:
{}", line);
        assert!(line.contains("_17 = _aggcp_bb2__17"), "dest bind:
{}", line);
    }

    #[test]
    fn agg_copy_skips_const_elems() {
        // Aggregates with Const elems do not go through lower_adt_init
        // with u64 slots, so no copy map entry (old behavior kept).
        let blocks = vec![Block {
            id: "bb0".to_string(),
            statements: vec![Stmt::Assign {
                dest: "_9".to_string(),
                dest_place: Some("_9".to_string()),
                rvalue: Rvalue::Aggregate {
                    elems: vec![
                        Operand::Move { place: "_10".to_string() },
                        Operand::Const { value: "Val(1)".to_string(), str_bytes: None, str_len: None },
                    ],
                    layout: Some(crate::mir::AdtLayout { size: 16, offsets: vec![0, 8] }),
                },
            }],
            terminator: Term::Return { ret: None },
        }];
        assert!(build_agg_copy(&blocks).is_empty());
    }


    #[test]
    fn const_dest_spill_slot_setup() {
        // 315-shape: a spilled dest defined by a plain scalar const
        // must still get its slot setup (the old Const fallback bound
        // no slot, so a later Copy use reloaded from an unbound slot).
        let mut spill = std::collections::BTreeMap::new();
        spill.insert("_1_v0".to_string(), "ptr".to_string());
        let mut unsup = vec![];
        let mut idx = 0usize;
        let line = render_rvalue(
            &Rvalue::Use {
                op: Operand::Const {
                    value: "Val(Scalar(0x00000002), u32)".to_string(),
                    str_bytes: None,
                    str_len: None,
                },
            },
            "_1_v0", Some("_1_v0"), &mut unsup, "bb2", &mut idx,
            &std::collections::HashMap::new(), &spill, &std::collections::BTreeMap::new(),
        );
        assert!(unsup.is_empty());
        assert!(line.contains("_1_v0_spill = alloc 8"), "slot alloc:\n{}", line);
        assert!(line.contains("store _1_v0_spill+0, _1_v0 as ptr"), "slot store:\n{}", line);
    }
}
