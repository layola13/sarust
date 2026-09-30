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

pub fn render_rvalue(
    rv: &Rvalue,
    dest: &str,
    dest_place: Option<&str>,
    unsup: &mut Vec<String>,
    bid: &str,
    mv_idx: &mut usize,
    cmap: &std::collections::HashMap<String, String>,
    spill: &crate::spill::SpillMap,
) -> String {
    match rv {
        Rvalue::Use { op } => {
            // Spilled shared values reload (never move the slot source).
            if let Operand::Copy { place } = op {
                if let Some(ty) = spill.get(place) {
                    return format!(
                        "{} = load {}+0 as {}",
                        dest,
                        crate::spill::spill_slot(place),
                        ty
                    );
                }
            }
            if let Some(t) = spill_reg(op, spill) {
                return format!("{} = {}", dest, t);
            }
            // Const-propagated literals re-materialize instead of moving a
            // shared temp (a second move of the temp would trap UseAfterMove).
            // Sound: literals carry no control or ownership dependence.
            match op {
                Operand::Move { place } | Operand::Copy { place } => {
                    if let Some(lit) = cmap.get(place) {
                        return format!("{} = {}", dest, lit);
                    }
                    format!("{} = {}", dest, render_operand(op))
                }
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
                if let Some(t) = spill_reg(&elems[0], spill) {
                    return format!(
                        "// single-elem aggregate (spill reload)\n{} = {}",
                        dest, t
                    );
                }
                format!("// single-elem aggregate (exact)\n{} = {}", dest, render_operand(&elems[0]))
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