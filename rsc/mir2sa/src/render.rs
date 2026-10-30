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
        Operand::Move { place , .. } | Operand::Copy { place , .. } => place,
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
    local_types: &std::collections::BTreeMap<String, String>,
) -> String {
    // Field-precision loud guard (lower() pre-checks the same predicate
    // and skips rendering, so in the real pipeline this never fires;
    // direct callers still get a loud marker, never a root-local value).
    if crate::render_util::rvalue_proj_loud(rv)
        || crate::render_util::rvalue_agg_field_copy_loud(rv)
    {
        unsup.push(format!("{}:{} FieldProjection", bid, dest));
        return format!(
            "// UNSUPPORTED field-projection -> {}: unrenderable projection",
            dest
        );
    }
    match rv {
        Rvalue::Use { op } => {
            // Field-precision: a projected read loads from the field
            // cell (scalar/pointer slot) instead of binding the root
            // local, and never consumes the base (a partial move keeps
            // the other fields live). Spilled dests keep their slot.
            if crate::render_util::operand_is_projected(op) {
                let mut lines =
                    crate::render_util::field_value_into(op, dest, bid, mv_idx);
                if let Some(t) = spill.get(dest) {
                    let slot = crate::spill::spill_slot(dest);
                    lines.push(format!("{} = alloc 8", slot));
                    lines.push(format!("store {}+0, {} as {}", slot, dest, t));
                }
                return lines.join("\n");
            }
            // Spilled shared values reload (never move the slot source).
            if let Operand::Copy { place , .. } = op {
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
                // uam-tail: a spilled dest keeps its own slot too
                // (same shape as the Copy-source branch above); the
                // old early return bound the reload without a slot,
                // so a later reload of this dest trapped UnknownRegister.
                let mut s = format!("{} = {}", dest, t);
                if let Some(dt) = spill.get(dest) {
                    let slot = crate::spill::spill_slot(dest);
                    s.push_str(&format!("\n{} = alloc 8\nstore {}+0, {} as {}", slot, slot, dest, dt));
                }
                return s;
            }
            // Copy of a multi-elem aggregate duplicates its slots (see
            // build_agg_copy): moving the base would consume the source
            // and a second Copy would trap UseAfterMove (308: _9 copied
            // into _17 and _18).
            if let Operand::Copy { place , .. } = op {
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
                Operand::Move { place , .. } | Operand::Copy { place , .. } => {
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
                // and bind its thin address.
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
                    // Slice-literal const in value position: inline the
                    // payload plus the (ptr,len) pair and bind the pair
                    // address (see layout::plan_str_pair). Spilled dests
                    // keep a slot exactly like the thin-address arm above:
                    // the pair address is a thin `ptr`, so Copy uses
                    // reload it instead of moving it twice.
                    if let Some(mut lines) = crate::layout::plan_str_pair(
                        dest, bid, *mv_idx, value, str_bytes.as_ref(), *str_len,
                    ) {
                        *mv_idx += 1;
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
        Rvalue::Ref { place, mut_, via, zst, off, off2, deref, deref_first, pty, .. } => {
            // Zero-sized borrows carry no data: null marker (exact).
            // Trailing `//` is ForbiddenSyntax: provenance goes full-line
            // (the Assign arm indents each line of multi-line returns).
            if *zst {
                return format!("// zero-sized borrow (no storage)\n{} = 0", dest);
            }
            // Field-precision: a field borrow points at the field cell
            // (scalar/reference fields) or at the buffer whose address
            // the slot stores (aggregate fields), never at the root.
            if *off != 0 || *deref {
                let mut lines = vec![];
                if *mut_ {
                    lines.push("// mut-borrow (Phase1: & + Referee)".to_string());
                }
                if let Some(v) = via {
                    lines.push(format!("// via {}", flat_comment(v)));
                }
                lines.append(&mut crate::render_util::field_addr_into(
                    place,
                    *off,
                    *off2,
                    *deref,
                    *deref_first,
                    pty.as_deref(),
                    dest,
                    bid,
                    mv_idx,
                ));
                if let Some(t) = spill.get(dest) {
                    let slot = crate::spill::spill_slot(dest);
                    lines.push(format!("{} = alloc 8", slot));
                    lines.push(format!("store {}+0, {} as {}", slot, dest, t));
                }
                return lines.join("\n");
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
            // Slice-literal call args materialize via a preamble (byte
            // buffer + pair) and pass the pair address; other args render
            // inline as before.
            let mut pre: Vec<String> = vec![];
            let mut a: Vec<String> = vec![];
            let mut bufs: Vec<String> = vec![];
            for arg in args {
                let (mut lines, text, mut b) =
                    crate::render_util::render_call_arg_preamble(arg, bid, mv_idx);
                pre.append(&mut lines);
                a.push(text);
                bufs.append(&mut b);
            }
            let mut s = format!("{} = call @{}({})", dest, sa_ident(func), a.join(", "));
            if !pre.is_empty() {
                s = format!("{}\n{}", pre.join("\n"), s);
            }
            // Free pair byte buffers right after the call (see
            // render_call_arg_preamble): they are never moved, so no
            // later pass frees them on normal-return paths.
            for b in bufs {
                s.push_str(&format!("\n!{}", b));
            }
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
                if let Some(ty) = spill.get(dest) {
                    let slot = crate::spill::spill_slot(dest);
                    lines.push(format!("{} = alloc 8", slot));
                    lines.push(format!("store {}+0, {} as {}", slot, dest, ty));
                }
                return lines.join("\n");
            }
            let (pl, lt) = bind_move_operand(left, bid, mv_idx);
            let (pr, rt) = bind_move_operand(right, bid, mv_idx);
            match binop_mnemonic(op).or_else(|| signed_binop_for(op, left, right, local_types)) {
                Some(m) => {
                    let mut lines = pl;
                    lines.extend(pr);
                    lines.push(format!("{} = {} {}, {}", dest, m, lt, rt));
                if let Some(ty) = spill.get(dest) {
                    let slot = crate::spill::spill_slot(dest);
                    lines.push(format!("{} = alloc 8", slot));
                    lines.push(format!("store {}+0, {} as {}", slot, dest, ty));
                }
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
                if let Some(ty) = spill.get(dest) {
                    let slot = crate::spill::spill_slot(dest);
                    lines.push(format!("{} = alloc 8", slot));
                    lines.push(format!("store {}+0, {} as {}", slot, dest, ty));
                }
                return lines.join("\n");
            }
            let mut idx = 0usize;
            let (pre, t) = bind_move_operand(operand, bid, &mut idx);
            match unop_mnemonic(op) {
                Some(m) => {
                    let mut lines = pre;
                    lines.push(format!("{} = {} {}", dest, m, t));
                if let Some(ty) = spill.get(dest) {
                    let slot = crate::spill::spill_slot(dest);
                    lines.push(format!("{} = alloc 8", slot));
                    lines.push(format!("store {}+0, {} as {}", slot, dest, ty));
                }
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
            let reload = if crate::render_util::operand_is_projected(op) {
                None
            } else {
                spill_reg(op, spill)
            };
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
            // Slice unsizing (spill-cast cluster): `&[T; N] -> &[T]` is
            // a fat pointer (data_ptr, N) in the slice.sal layout
            // (ptr +0, len +8; PtrMetadata reads +8). The truncated
            // `src_ty` field ("&") cannot supply N; the source local's
            // driver type in local_types can. A spill reload cannot be
            // nested in a store, so bind it to a temp first (same rule
            // as the Convert arm). Dyn targets, fn-pointer coercions and
            // anything else fall through to lower_cast / loud unchanged.
            let fat_len = match (castkind.as_deref(), op.as_ref()) {
                (Some(k), Operand::Move { place, .. } | Operand::Copy { place, .. })
                    if !crate::render_util::operand_is_projected(op.as_ref()) => {
                    local_types
                        .get(place)
                        .or_else(|| local_types.get(crate::asm::strip_version_suffix(place)))
                        .and_then(|t| crate::asm::unsize_slice_len(k, t, ty))
                }
                _ => None,
            };
            if let Some(len) = fat_len {
                let thin = match &reload {
                    Some(t) => {
                        let tmp = format!("_mv_{}_{}", bid, mv_idx);
                        *mv_idx += 1;
                        pre.push(format!("{} = {}", tmp, t));
                        tmp
                    }
                    None => ot.clone(),
                };
                let fat = format!("_fat_{}_{}", bid, dest);
                pre.push(format!("{} = alloc 16", fat));
                pre.push(format!("store {}+0, {} as ptr", fat, thin));
                pre.push(format!("store {}+8, {} as u64", fat, len));
                pre.push(format!("{} = {}", dest, fat));
                slot(&mut pre);
                return pre.join("\n");
            }
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
        Rvalue::RawPtr { place, off, off2, deref, deref_first, pty, .. } => {
            // Field-precision: a raw pointer to a field uses the field
            // address, not the root local (same rule as Ref).
            if *off != 0 || *deref {
                return crate::render_util::field_addr_into(
                    place,
                    *off,
                    *off2,
                    *deref,
                    *deref_first,
                    pty.as_deref(),
                    dest,
                    bid,
                    mv_idx,
                )
                .join("\n");
            }
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
                // Field-precision: a projected elem loads its field
                // value (a spill reload would bind the base pointer).
                let mut s = if crate::render_util::operand_is_projected(&elems[0]) {
                    let lines = crate::render_util::field_value_into(
                        &elems[0], dest, bid, mv_idx,
                    );
                    format!(
                        "// single-elem aggregate (field)\n{}",
                        lines.join("\n")
                    )
                } else if let Some(t) = spill_reg(&elems[0], spill) {
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
                    // uam-tail: a spilled multi-elem Aggregate dest
                    // keeps its own slot too, so later field Copy or
                    // Move uses reload instead of moving the buffer.
                    if let Some(ty) = spill.get(dest) {
                        let slot = crate::spill::spill_slot(dest);
                        lines.push(format!("{} = alloc 8", slot));
                        lines.push(format!("store {}+0, {} as {}", slot, dest, ty));
                    }
                    lines.join("\n")
                } else if let Some(mut lines) = lower_adt_init(bid, dest, place, elems, layout.as_ref()) {
                    crate::spill::spill_slot_lines(&mut lines, bid, spill);
                    // uam-tail: a spilled multi-elem Aggregate dest
                    // keeps its own slot too, so later field Copy or
                    // Move uses reload instead of moving the buffer.
                    if let Some(ty) = spill.get(dest) {
                        let slot = crate::spill::spill_slot(dest);
                        lines.push(format!("{} = alloc 8", slot));
                        lines.push(format!("store {}+0, {} as {}", slot, dest, ty));
                    }
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
                        Operand::Move { place: "_10".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false },
                        Operand::Move { place: "_11".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false },
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
            &Rvalue::Use { op: Operand::Copy { place: "_9".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false } },
            "_17", Some("_17"), &mut unsup, "bb2", &mut idx,
            &std::collections::HashMap::new(), &std::collections::BTreeMap::new(), &map,
            &std::collections::BTreeMap::new(),
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
                        Operand::Move { place: "_10".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false },
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
    fn binop_dest_spill_slot_setup() {
        // uam-tail (188): a BinOp-defined temp that is spilled must
        // get its slot setup in the BinOp arm, or a later plain-assign
        // Copy use reloads from an unbound slot.
        let mut spill = std::collections::BTreeMap::new();
        spill.insert("_2".to_string(), "u8".to_string());
        let mut unsup = vec![];
        let mut idx = 0usize;
        let line = render_rvalue(
            &Rvalue::BinOp {
                op: "BitAnd".to_string(),
                left: Box::new(Operand::Move { place: "_3".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false }),
                right: Box::new(Operand::Const { value: "Val(Scalar(0x80), u8)".to_string(), str_bytes: None, str_len: None }),
            },
            "_2",
            Some("_2"),
            &mut unsup,
            "bb0",
            &mut idx,
            &std::collections::HashMap::new(),
            &spill,
            &std::collections::BTreeMap::new(),
            &std::collections::BTreeMap::new(),
        );
        assert!(line.contains("_2_spill = alloc 8"), "slot alloc:
{}", line);
        assert!(line.contains("store _2_spill+0, _2 as u8"), "slot store:
{}", line);
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
            &std::collections::BTreeMap::new(),
        );
        assert!(unsup.is_empty());
        assert!(line.contains("_1_v0_spill = alloc 8"), "slot alloc:\n{}", line);
        assert!(line.contains("store _1_v0_spill+0, _1_v0 as ptr"), "slot store:\n{}", line);
    }

    #[test]
    fn signed_binop_renders_true_values() {
        // C4: ordered/div/rem/shift binops lower to signedness-resolved
        // SA mnemonics when both operand types are known and agree;
        // unknown, 128-bit, float and mismatched-signedness operands
        // stay loud (never guessed).
        let lt: std::collections::BTreeMap<String, String> = [
            ("_1".to_string(), "i32".to_string()),
            ("_2".to_string(), "u8".to_string()),
            ("_3".to_string(), "u128".to_string()),
            ("_4".to_string(), "f64".to_string()),
            ("_5".to_string(), "usize".to_string()),
        ]
        .into_iter()
        .collect();
        let bin = |op: &str, left: Operand, right: Operand| {
            let mut unsup = vec![];
            let mut idx = 0usize;
            let line = render_rvalue(
                &Rvalue::BinOp {
                    op: op.to_string(),
                    left: Box::new(left),
                    right: Box::new(right),
                },
                "_9",
                Some("_9"),
                &mut unsup,
                "bb0",
                &mut idx,
                &std::collections::HashMap::new(),
                &std::collections::BTreeMap::new(),
                &std::collections::BTreeMap::new(),
                &lt,
            );
            (line, unsup)
        };
        let c = |v: &str| Operand::Const {
            value: v.to_string(),
            str_bytes: None,
            str_len: None,
        };
        let cp = |p: &str| Operand::Copy {
            place: p.to_string(),
        off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false };
        let mv = |p: &str| Operand::Move {
            place: p.to_string(),
        off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false };
        // Signed comparison (24-factorial shape): Le(i32, 1_i32).
        let (line, unsup) = bin("Le", cp("_1"), c("Val(Scalar(0x00000001), i32)"));
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(line, "_9 = sle _1, 1");
        // Versioned place strips to its base local.
        let (line, unsup) = bin("Le", cp("_1_v0"), c("Val(Scalar(0x00000001), i32)"));
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(line, "_9 = sle _1_v0, 1");
        // Signed comparison with a Move operand binds a temp first.
        let (line, unsup) = bin("Gt", mv("_1"), c("Val(Scalar(0x00000005), i32)"));
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(line, "_mv_bb0_0 = _1\n_9 = sgt _mv_bb0_0, 5");
        // Unsigned div/rem (190-base64 shape): Div/Rem(u8, 4_u8).
        let (line, unsup) = bin("Div", mv("_2"), c("Val(Scalar(0x04), u8)"));
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(line, "_mv_bb0_0 = _2\n_9 = udiv _mv_bb0_0, 4");
        let (line, unsup) = bin("Rem", mv("_2"), c("Val(Scalar(0x04), u8)"));
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(line, "_mv_bb0_0 = _2\n_9 = urem _mv_bb0_0, 4");
        // Unsigned comparison (usize).
        let (line, unsup) = bin("Lt", cp("_5"), c("Val(Scalar(0x00000020), usize)"));
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(line, "_9 = ult _5, 32");
        // Shr reads only its left operand: signed -> ashr, unsigned -> lshr.
        let (line, unsup) = bin("Shr", mv("_1"), c("Val(Scalar(0x00000002), u32)"));
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(line, "_mv_bb0_0 = _1\n_9 = ashr _mv_bb0_0, 2");
        let (line, unsup) = bin("Shr", mv("_5"), c("Val(Scalar(0x00000002), u32)"));
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(line, "_mv_bb0_0 = _5\n_9 = lshr _mv_bb0_0, 2");
        // Loud: 128-bit, float, mismatched signedness, unknown place.
        for (op, left, right) in [
            ("Ge", cp("_3"), cp("_3")),
            ("Le", cp("_4"), c("Val(Scalar(0x0000000000000000), f64)")),
            ("Le", cp("_1"), c("Val(Scalar(0x00000001), u32)")),
            ("Le", cp("_9"), c("Val(Scalar(0x00000001), i32)")),
        ] {
            let (line, unsup) = bin(op, left, right);
            assert!(!unsup.is_empty(), "{} should stay loud: {}", op, line);
            assert!(line.starts_with("// UNSUPPORTED binop"), "{}", line);
        }
        // Coverage mirror agrees with render.
        assert!(crate::render_util::binop_emittable(
            "Le",
            &cp("_1"),
            &c("Val(Scalar(0x00000001), i32)"),
            &lt
        ));
        assert!(!crate::render_util::binop_emittable("Ge", &cp("_3"), &cp("_3"), &lt));
    }

    #[test]
    fn slice_unsize_cast_builds_fat_pair() {
        // Spill-cast cluster: `&[i32; 4] -> &[i32]` lowers to the
        // (ptr, len) pair, not a loud 0 placeholder. Spill reloads bind
        // to a temp first (a load cannot nest inside a store).
        let spilled = std::collections::BTreeMap::from([("_5".to_string(), "ptr".to_string())]);
        let tys = std::collections::BTreeMap::from([(
            "_5".to_string(),
            "&'{erased} [i32; 4_usize]".to_string(),
        )]);
        let cast = Rvalue::Cast {
            op: Box::new(Operand::Move { place: "_5".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false }),
            ty: "&'{erased} [i32]".to_string(),
            castkind: Some("PointerCoercion(Unsize, Implicit)".to_string()),
            src_ty: Some("&".to_string()),
        };
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = render_rvalue(
            &cast, "_4", Some("_4"), &mut unsup, "bb0", &mut idx,
            &std::collections::HashMap::new(), &spilled, &std::collections::BTreeMap::new(), &tys,
        );
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(
            line,
            "_mv_bb0_0 = load _5_spill+0 as ptr\n_fat_bb0__4 = alloc 16\nstore _fat_bb0__4+0, _mv_bb0_0 as ptr\nstore _fat_bb0__4+8, 4 as u64\n_4 = _fat_bb0__4"
        );
        // Non-spill Copy source: the store reads the place directly
        // (non-consuming), no temp needed.
        let cast_copy = Rvalue::Cast {
            op: Box::new(Operand::Copy { place: "_5".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false }),
            ty: "&'{erased} [i32]".to_string(),
            castkind: Some("PointerCoercion(Unsize, Implicit)".to_string()),
            src_ty: Some("&".to_string()),
        };
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = render_rvalue(
            &cast_copy, "_4", Some("_4"), &mut unsup, "bb0", &mut idx,
            &std::collections::HashMap::new(), &std::collections::BTreeMap::new(), &std::collections::BTreeMap::new(), &tys,
        );
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert!(line.contains("_fat_bb0__4 = alloc 16"), "{}", line);
        assert!(line.contains("store _fat_bb0__4+0, _5 as ptr"), "{}", line);
        assert!(line.contains("store _fat_bb0__4+8, 4 as u64"), "{}", line);
        // A dyn target keeps its loud placeholder (vtable, not a length).
        let dyn_cast = Rvalue::Cast {
            op: Box::new(Operand::Move { place: "_5".to_string() , off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false }),
            ty: "&'{erased} dyn [Binder { value: Trait(Draw), bound_vars: [] }]".to_string(),
            castkind: Some("PointerCoercion(Unsize, Implicit)".to_string()),
            src_ty: Some("&".to_string()),
        };
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = render_rvalue(
            &dyn_cast, "_3", Some("_3"), &mut unsup, "bb0", &mut idx,
            &std::collections::HashMap::new(), &std::collections::BTreeMap::new(), &std::collections::BTreeMap::new(), &tys,
        );
        assert!(!unsup.is_empty());
        assert!(line.contains("UNSUPPORTED cast"), "{}", line);
    }
}
