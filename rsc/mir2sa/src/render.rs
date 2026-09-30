//! mir2sa::render.
use crate::asm::{cast_dst_short, lower_cast, CastLower};
use crate::const_util::{repeat_plan, sa_scalar_ty};
use crate::layout::{lower_adt_init, lower_array_init, tls_key};
use crate::mir::*;
use crate::render_util::*;

pub fn render_rvalue(
    rv: &Rvalue,
    dest: &str,
    dest_place: Option<&str>,
    unsup: &mut Vec<String>,
    bid: &str,
    mv_idx: &mut usize,
) -> String {
    match rv {
        Rvalue::Use { op } => format!("{} = {}", dest, render_operand(op)),
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
            lines.join("\n")
        }
        Rvalue::Call { func, args, sig } => {
            if call_sig_loud(sig, args) {
                unsup.push(format!("{}:{} CallNoSig", bid, dest));
                return format!("// UNSUPPORTED call-sig -> {}: unresolvable callee signature", dest);
            }
            let a: Vec<String> = args.iter().map(render_call_arg).collect();
            format!("{} = call @{}({})", dest, sa_ident(func), a.join(", "))
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
            let (mut pre, ot) = bind_move_operand(op, bid, mv_idx);
            // SA emission type for conversions (`as TY`).
            let emit_ty = if dst == "ptr" {
                "ptr".to_string()
            } else {
                sa_scalar_ty(dst.trim()).to_string()
            };
            match (castkind.as_deref(), src_ty.as_deref()) {
                (Some(k), Some(s)) => match lower_cast(k, s, &dst) {
                    Some(CastLower::Copy) => {
                        pre.push(format!("{} = {}", dest, ot));
                        pre.join("\n")
                    }
                    Some(CastLower::Convert(m)) => {
                        pre.push(format!("{} = {} {} as {}", dest, m, ot, emit_ty));
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
                    Some((v, total)) => format!(
                        "// repeat [{}; {}]\n_rep_{} = alloc {}\ncall @sa_mem_set(&_rep_{}, {}, {})\n{} = _rep_{}",
                        value, len, bid, total, bid, v, total, dest, bid
                    ),
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
                format!("// single-elem aggregate (exact)\n{} = {}", dest, render_operand(&elems[0]))
            } else if let Some(place) = dest_place {
                // Array fast path first (exact [T; N] all-const, incl. ManuallyDrop backing).
                if let Some(lines) = lower_array_init(bid, place, elems) {
                    lines.join("\n")
                } else if let Some(lines) = lower_adt_init(bid, dest, place, elems, layout.as_ref()) {
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