//! mir2sa::lower.
use std::process::ExitCode;

use crate::asm::{asm_inout_passthrough, asm_mov_copy, is_plain_local};
use crate::mir::*;
use crate::render::*;
use crate::render_util::{assert_panic_code, assign_loud_const, build_constmap, call_sig_loud, const_needs_loud, flat_comment, is_overflow_assert, loud_const_kind, loud_const_kind_call, loud_operand_reason, render_call_arg, render_operand, sa_ident, sa_label, unreachable_panic_code};
use crate::spill::{build_spill, spill_slot};

/// Placeholder bind for loud paths: an unbound dest gets `= 0` so
/// downstream uses stay parseable (the function is already flagged loud).
/// Already-bound dests keep their first value (no-op here).
pub fn bind_placeholder(
    out: &mut Vec<String>,
    bound: &mut std::collections::BTreeSet<String>,
    dest: &Option<String>,
) {
    if let Some(d) = dest {
        if !bound.contains(d) {
            out.push(format!("    {} = 0", d));
            bound.insert(d.clone());
        }
    }
}

/// Mirror a placeholder bind into the spill slot (keeps reloads parseable).
/// No-op when the dest is not spilled.
pub fn spill_placeholder(
    out: &mut Vec<String>,
    spill: &crate::spill::SpillMap,
    dest: &Option<String>,
) {
    if let Some(d) = dest {
        if let Some(ty) = spill.get(d) {
            let slot = crate::spill::spill_slot(d);
            out.push(format!("    {} = alloc 8", slot));
            out.push(format!("    store {}+0, {} as {}", slot, d, ty));
        }
    }
}

/// Phase-5 end lines for one JoinEnd, plus the spill-slot free when
/// the ended register is spilled (see the 30 leak note above).
fn join_end_lines(e: &crate::borrow_end::JoinEnd, spill: &crate::spill::SpillMap) -> Vec<String> {
    let mut v = e.lines();
    if spill.contains_key(e.reg()) {
        v.push(format!("    !{}_spill", e.reg()));
    }
    v
}

pub fn lower_function_with_layouts(
    f: &Function,
    unsup: &mut Vec<String>,
    layouts: &crate::place_store::AdtLayoutIndex,
) -> String {    // Typed header: MIR arg locals `_1..` bind here (bare `@f()` would leave
    // them unbound: UnknownRegister). Ret defaults to i32 (old fixtures).
    let mut head = format!("@{}(", sa_ident(&f.name));
    for (i, t) in f.params.iter().enumerate() {
        if i > 0 {
            head.push_str(", ");
        }
        head.push_str(&format!("_{}: {}", i + 1, t));
    }
    head.push_str(&format!(") -> {}:", f.ret.as_deref().unwrap_or("i32")));
    let mut out = vec![head];
    // Unrepresentable signature parts fall back to `ptr` above: counted loud
    // here to mirror coverage (parse-clean output, honest accounting).
    if !f.sig_ok {
        unsup.push("FnSig".to_string());
        out.push("    // UNSUPPORTED fn-sig: unrepresentable param/return type (ptr fallback)".to_string());
    }
    // RPO emission order (see order.rs): SA checks def-before-use textually.
    // Bound seeds come from dominators (see order::dom_seeds): a rebind of
    // a seeded dest is a same-path redefinition and goes loud.
    let seeds = crate::order::dom_seeds(&f.blocks, f.params.len());
    // Borrow-end plan (see borrow_end.rs): NLL-dead borrowers end just
    // before their source is dropped. Pure MIR-level, shared with coverage.
    // The spill map is built first: the plan needs the spilled set so
    // plain-assign reloads are not counted as consuming events.
    let doms = crate::order::dom_sets(&f.blocks);
    let spill = build_spill(&f.blocks, &f.local_types);
    let spilled: std::collections::BTreeSet<String> = spill.keys().cloned().collect();
    let borrow_plan = crate::borrow_end::plan_drops(&f.blocks, &doms, &spilled);
    // Phi slots (C2 version-conflict): per conflicted base local, one
    // entry-allocated slot that every versioned def stores into; conflict
    // uses load from it (see phi.rs for the soundness gates).
    let reachable = crate::order::reachable_blocks(&f.blocks);
    let phi = crate::phi::build_phi(&f.blocks, &f.local_types, f.ret.as_deref(), &spill, &reachable);
    let phi_empty: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    let phi_returns = crate::phi::return_blocks(&f.blocks);
    // Aggregate-copy map (phi-11, 308): multi-elem Aggregate defs whose
    // elems are all Move/Copy (see render::build_agg_copy).
    let agg_copy = crate::render::build_agg_copy(&f.blocks);
    // Block line ranges (for drop-glue insertion afterwards).
    let mut block_ranges: Vec<(usize, usize, usize)> = vec![];
    // Const-propagation map (RPO order: defs precede dominated uses).
    // Literals re-materialize at use sites instead of moving shared temps.
    let order = crate::order::rpo_order(&f.blocks);
    let mut cmap = build_constmap(
        order
            .iter()
            .flat_map(|bi| f.blocks[*bi].statements.iter().map(move |st| (f.blocks[*bi].id.as_str(), st))),
    );
    // Locals defined but never read: a byte-literal store into one of them
    // would allocate a payload nobody can free (see spill::dead_locals).
    let dead = crate::spill::dead_locals(&f.blocks, f.params.len());
    // Block ranges close when the NEXT block starts, never inside the loop:
    // a loud terminator arm `continue`s, and its lines still define registers
    // that drop.rs must see (missing them under-freed every exit, which the
    // Referee reports as a leak as soon as a register holds a real
    // allocation). Opening here keeps the range closable after a `continue`.
    // Entry block for param spill-slot setup: MIR bb0 when present.
    let entry_bi = f.blocks.iter().position(|b| b.id == "bb0").unwrap_or(order.first().copied().unwrap_or(0));
    let mut open_block: Option<usize> = None;
    let mut block_start = vec![0usize; f.blocks.len()];
    for bi in order {
        let b = &f.blocks[bi];
        let start = out.len();
        if let Some(prev) = open_block.take() {
            block_ranges.push((prev, block_start[prev], out.len()));
        }
        block_start[bi] = start;
        out.push(format!("{}:", sa_label(&b.id)));
        open_block = Some(bi);
        // Unreachable (cleanup) blocks never resolve phi items (phi.rs):
        // they use the empty map, so their conflict uses keep the loud
        // path and their defs never store to the slot.
        let phi_b: &std::collections::BTreeMap<String, String> =
            if reachable.contains(&bi) { &phi } else { &phi_empty };
        // Param spill slots (phi-11): header-bound params never pass
        // through a render arm, so their slot setup goes at the entry
        // block, before any Copy use can reload from it. Statement and
        // call dests set their own slots up in their render arms.
        if bi == entry_bi {
            for i in 1..=f.params.len() {
                let name = format!("_{}", i);
                if let Some(ty) = spill.get(&name) {
                    let slot = spill_slot(&name);
                    out.push(format!("    {} = alloc 8", slot));
                    out.push(format!("    store {}+0, {} as {}", slot, name, ty));
                }
            }
            // Phi slots: alloc once here (before any def store or
            // conflict load can run), zero scalar slots so an
            // unreached/UB load is deterministic, and seed param locals
            // with the header-bound value so pre-def param uses (bb0
            // conflicts) load the param. Phi locals never collide with
            // the spill map (build_phi excludes spilled bases).
            for (local, ty) in phi_b {
                let slot = spill_slot(local);
                out.push(format!("    {} = alloc 8", slot));
                if ty != "ptr" {
                    out.push(format!("    store {}+0, 0 as {}", slot, ty));
                }
            }
            for i in 1..=f.params.len() {
                let name = format!("_{}", i);
                if let Some(ty) = phi_b.get(&name) {
                    out.push(format!("    store {}+0, {} as {}", spill_slot(&name), name, ty));
                }
            }
        }
        // Per-block bound set: repeats go loud and keep the first value
        // (skip the line). Exclusive-branch joins stay legal (never seeded).
        let mut bound = seeds[bi].clone();
        let mut mv_idx = 0usize;
        // Shared-borrow phi temps need a return-dominating block (phi.rs).
        let ref_ok = crate::phi::phi_ref_allowed(&doms, &phi_returns, bi);
        for st in &b.statements {
            match st {
                Stmt::Assign { dest, dest_place, rvalue } => {
                    // Trailing `//` is ForbiddenSyntax: the Assign arm never
                    // appends provenance comments (they live in mir.json).
                    // Place store (C5): a store through an already-bound
                    // base is not a redefinition — lower the store, bind
                    // nothing, and leave phi/spill slots untouched.
                    if bound.contains(dest) {
                        if let Some(lines) = crate::place_store::emit_place_store(
                            dest_place.as_deref(),
                            dest,
                            rvalue,
                            f,
                            layouts,
                            phi_b,
                            &spill,
                            &cmap,
                            &b.id,
                            &mut mv_idx,
                        ) {
                            for l in lines {
                                out.push(format!("    {}", l));
                            }
                            continue;
                        }
                    }
                    if bound.contains(dest) {
                        unsup.push(format!("{}:{} Rebind", b.id, dest));
                        out.push(format!("    // UNSUPPORTED rebind -> {} (already bound; keeping first value)", dest));
                        continue;
                    }
                    // Phi rewrite (C2): conflict uses become slot loads
                    // into fresh temps; unresolvable phi items fall back
                    // to the original rvalue and its loud path (never a
                    // partial rewrite).
                    let (rvalue_rw, phi_loads, phi_frees) =
                        match crate::phi::rewrite_rvalue(rvalue, phi_b, &b.id, &mut mv_idx, ref_ok) {
                            Some(v) => v,
                            None => (rvalue.clone(), vec![], vec![]),
                        };
                    let rvalue = &rvalue_rw;
                    // Unresolvable phi places (mut borrows, non-dominating
                    // shared borrows, unmappable types) go loud here: the
                    // marker must never reach render_rvalue, which would
                    // emit it literally as a register name (UnknownRegister).
                    let phi_place_loud = matches!(
                        rvalue,
                        Rvalue::Ref { place, .. }
                            | Rvalue::Discriminant { place }
                            | Rvalue::RawPtr { place, .. }
                        if crate::phi::phi_place(place).is_some()
                    );
                    // Unresolvable const values go loud (never raw Debug).
                    let before = unsup.len();
                    // A dead byte-literal store allocates a payload that
                    // nothing can read or free; skip the definition (no use
                    // can reach the unbound register).
                    let dead_literal = dead.contains(dest)
                        && matches!(
                            rvalue,
                            Rvalue::Use { op: Operand::Const { str_bytes: Some(_), .. } }
                        );
                    if phi_place_loud {
                        // Same shape as the old Unsupported rvalue arm:
                        // one loud marker, then the placeholder path below.
                        let base = match rvalue {
                            Rvalue::Ref { place, .. }
                            | Rvalue::Discriminant { place }
                            | Rvalue::RawPtr { place, .. } => {
                                crate::phi::phi_place(place).unwrap_or("?")
                            }
                            _ => "?",
                        };
                        unsup.push(format!("{}:{} Unsupported", b.id, dest));
                        out.push(format!("    // UNSUPPORTED rvalue -> {}: version conflict at join (multiple reaching defs of {})", dest, base));
                    } else if dead_literal {
                        out.push(format!("    // dead byte-literal store {} (no reads; not materialized)", dest));
                    } else if let Some(why) = assign_loud_const(rvalue) {
                        unsup.push(format!("{}:{} ConstValue", b.id, dest));
                        out.push(format!("    // UNSUPPORTED const-value -> {}: {}", dest, flat_comment(&why)));
                    } else if crate::render_util::rvalue_proj_loud(rvalue)
                        || crate::render_util::rvalue_agg_field_copy_loud(rvalue)
                    {
                        // Field-precision: same shape as the const branch
                        // (one marker; the post-chain placeholder binds
                        // dest = 0 below). render_rvalue is never reached
                        // for these, so there is no double count.
                        unsup.push(format!("{}:{} FieldProjection", b.id, dest));
                        out.push(format!("    // UNSUPPORTED field-projection -> {}: unrenderable projection", dest));
                    } else {
                        for l in &phi_loads {
                            out.push(format!("    {}", l));
                        }
                        let line = render_rvalue(rvalue, dest, dest_place.as_deref(), unsup, &b.id, &mut mv_idx, &cmap, &spill, &agg_copy, &f.local_types);
                        for l in line.split('\n') {
                            out.push(format!("    {}", l));
                        }
                        for t in &phi_frees {
                            out.push(format!("    !{}", t));
                        }
                    }
                    if unsup.len() > before && !dead_literal {
                        // Loud paths bind nothing: a `0` placeholder keeps
                        // downstream uses parseable (function already flagged).
                        // Feed the constmap so later Copies re-materialize
                        // the marker instead of moving it (UseAfterMove).
                        out.push(format!("    {} = 0", dest));
                        cmap.insert(dest.clone(), "0".to_string());
                        // Spilled dests mirror the placeholder into the slot
                        // so reloads stay parseable too.
                        spill_placeholder(&mut out, &spill, &Some(dest.clone()));
                    }
                    bound.insert(dest.clone());
                    // Phi def store: every versioned def of a phi local
                    // refreshes the slot (skipped for dead literals, whose
                    // dest binds nothing; rebinds `continue` above, and
                    // their first value is already in the slot).
                    if !dead_literal {
                        let base = crate::asm::strip_version_suffix(dest).to_string();
                        if let Some(ty) = phi_b.get(&base) {
                            out.push(format!("    store {}+0, {} as {}", spill_slot(&base), dest, ty));
                        }
                    }
                }
                Stmt::StorageLive { local } => out.push(format!("    // StorageLive {}", local)),
                Stmt::StorageDead { local } => out.push(format!("    // StorageDead {}", local)),
                Stmt::Nop { text } => out.push(format!("    // nop: {}", text)),
                Stmt::SetDisc { place, variant, .. } => {
                    // Phi set-discriminant (C2): the place holds a pointer
                    // to the enum buffer, so storing the tag through a
                    // slot-loaded temp writes the real buffer.
                    if let Some(base) = crate::phi::phi_place(place) {
                        if phi.get(base).is_some_and(|t| t == "ptr") {
                            let tmp = format!("_phi_{}_{}", b.id, mv_idx);
                            mv_idx += 1;
                            out.push(format!("    {} = load {}+0 as ptr", tmp, spill_slot(base)));
                            out.push(format!("    store {}+0, {} as i64", tmp, variant));
                            out.push(format!("    !{}", tmp));
                        } else {
                            unsup.push(format!("{}: UnsupportedStmt", b.id));
                            out.push(format!("    // UNSUPPORTED stmt: version conflict at join (multiple reaching defs of {})", base));
                        }
                    } else {
                    // p_layout v1: enum tag lives at offset 0 (sla enum_tag_offset=0,
                    // payload at 8). Driver reports the base local already; the
                    // full place (`(*_9)`) is kept as a comment for provenance.
                    out.push("    // set-discriminant (enum tag)".to_string());
                    if let Some(ty) = spill.get(place) {
                        // Spilled base: reload through a temp so the store
                        // survives an earlier plain-assign move of the
                        // original register (the UseAfterMove this gate fixes).
                        let tmp = format!("_mv_{}_{}", b.id, mv_idx);
                        mv_idx += 1;
                        let slot = crate::spill::spill_slot(place);
                        out.push(format!("    {} = load {}+0 as {}", tmp, slot, ty));
                        out.push(format!("    store {}+0, {} as i64", tmp, variant));
                    } else {
                        out.push(format!("    store {}+0, {} as i64", place, variant));
                    }
                    }
                }
                Stmt::UnsupportedStmt { text } => {
                    unsup.push(format!("{}: UnsupportedStmt", b.id));
                    out.push(format!("    // UNSUPPORTED stmt: {}", text));
                }
            }
        }
        // Merge ends (borrow_end phase 3): a double-borrow chain dead at
        // a join ends here, leaf first, so the join sees its source
        // unlocked. Drop-glue below sees these `!` lines as ordinary
        // frees and never re-frees the chain at an exit.
        if let Some(ends) = borrow_plan.merge_ends.get(&bi) {
            for r in ends {
                out.push(format!("    !{}", r));
            }
        }
        // Loop back-edge ends (borrow_end phase 4): borrows dead
        // before a loop back-edge end here, so the next iteration's
        // merge at the loop head sees their sources unlocked. Kept
        // separate from merge ends: the exit path still needs (and
        // keeps) its own phase-1/drop ends for the same registers.
        if let Some(ends) = borrow_plan.loop_ends.get(&bi) {
            for r in ends {
                out.push(format!("    !{}", r));
            }
        }
        // Join ends (borrow_end phase 5) for non-Call blocks: def-block
        // borrower ends, then join-consume ends, before the terminator
        // renders. Call blocks take theirs at the drop-glue point below
        // (after the call, so call args are still live). Ordering here
        // is borrower ends before source consumes by construction.
        if let Some(ends) = borrow_plan.block_ends.get(&bi) {
            for e in ends {
                for l in join_end_lines(e, &spill) {
                    out.push(l);
                }
            }
        }
        match &b.terminator {
            Term::Goto { target } => out.push(format!("    jmp {}", sa_label(target))),
            Term::Return { ret } => {
                // Phi return (C2): scalar conflict returns load the slot
                // and return the temp (consumed by the return); pointer
                // returns stay loud (see phi.rs).
                let phi_ret = match ret.as_deref().and_then(crate::phi::phi_place) {
                    Some(base) => match phi.get(base) {
                        Some(ty) if ty != "ptr" => Some(ty.clone()),
                        _ => None,
                    },
                    None => None,
                };
                if let Some(ty) = phi_ret {
                    let base = ret.as_deref().and_then(crate::phi::phi_place).unwrap();
                    // No mv_idx increment: the return temp is the block's
                    // last temp use, and its mv_idx value is still fresh.
                    let tmp = format!("_phi_{}_{}", b.id, mv_idx);
                    out.push(format!("    {} = load {}+0 as {}", tmp, spill_slot(base), ty));
                    out.push(format!("    return {}", tmp));
                } else {
                // Return-value fidelity: `return <reg>` is legal and consumes
                // the reg (probe r4; `return 0` leaks it, probe r5). Fall back
                // to the `0` marker when unresolved, void, conflicting, or not
                // definitely bound here — the marker is always parse-clean.
                let is_void = f.ret.as_deref() == Some("void");
                let live = ret.as_deref().filter(|_| !is_void).filter(|r| {
                    crate::phi::phi_place(r).is_none() && bound.contains(*r)
                });
                match live {
                    Some(r) => out.push(format!("    return {}", r)),
                    None => {
                        if ret.as_deref().is_some_and(|r| crate::phi::phi_place(r).is_some() || r == "__VERSION_CONFLICT__") {
                            unsup.push(format!("{}: ReturnConflict", b.id));
                            out.push("    // UNSUPPORTED return-value: version conflict at join (multiple reaching defs)".to_string());
                            out.push("    return 0".to_string());
                        } else if ret.is_none() && !is_void && bound.contains("_0") {
                            // Single-def fast path (version.rs) leaves ret
                            // unset even though _0 is bound: returning it
                            // is exact (drop glue vetoes freeing registers
                            // read by `return` lines).
                            out.push("    return _0".to_string());
                        } else {
                            out.push("    return 0".to_string());
                        }
                    }
                }
                }
            }
            Term::Resume => {
                out.push("    // MIR Resume (cleanup path)".to_string());
                out.push("    panic(\"unwind-resume\")".to_string());
            }
            Term::Call { func, func_raw, args, dest, target, sig } => {
                // Phi rewrite (C2): conflict call args become slot loads.
                // Unresolvable args fall back to the original args and
                // their loud path (never a partial rewrite).
                let (args_rw, phi_loads, phi_frees) = {
                    let mut na: Vec<Operand> = vec![];
                    let mut loads: Vec<String> = vec![];
                    let mut frees: Vec<String> = vec![];
                    let mut ok = true;
                    for arg in args.iter() {
                        match crate::phi::rewrite_operand(arg, phi_b, &b.id, &mut mv_idx) {
                            Some((n, l, t)) => {
                                if let Some(l) = l {
                                    loads.push(l);
                                }
                                if let Some(t) = t {
                                    frees.push(t);
                                }
                                na.push(n);
                            }
                            None => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if ok {
                        (na, loads, frees)
                    } else {
                        (args.clone(), vec![], vec![])
                    }
                };
                let args = &args_rw;
                if let Some(raw) = func_raw {
                    out.push(format!("    // MIR: {}", flat_comment(raw)));
                }
                // Rebound call dests keep the first value (bare call preserves
                // side effects; control preserved below).
                let rebind = dest.as_deref().is_some_and(|d| bound.contains(d));
                if rebind {
                    unsup.push(format!("{}: Rebind", b.id));
                    out.push(format!("    // UNSUPPORTED rebind -> {} (already bound; keeping first value)", dest.as_deref().unwrap_or("_0")));
                }
                if let Some(kind) = args.iter().find_map(loud_const_kind_call) {
                    unsup.push(format!("{}: Call{}", b.id, kind));
                    out.push(format!(
                        "    // UNSUPPORTED call-args: {} (control preserved)",
                        loud_operand_reason(args.iter().find(|a| loud_const_kind_call(a).is_some()).unwrap()).unwrap_or_default()
                    ));
                    bind_placeholder(&mut out, &mut bound, dest);
                    spill_placeholder(&mut out, &spill, dest);
                    match target {
                        Some(t) => out.push(format!("    jmp {}", sa_label(t))),
                        None => out.push(format!("    panic({})", unreachable_panic_code(&b.id))),
                    }
                    continue;
                }
                // Field-precision: an unrenderable projection in any
                // call arg goes loud (control preserved), same shape as
                // the const gate above.
                if args.iter().any(|a| crate::render_util::operand_proj_loud(a)) {
                    unsup.push(format!("{}: CallFieldProjection", b.id));
                    out.push("    // UNSUPPORTED call-args: unrenderable field projection (control preserved)".to_string());
                    bind_placeholder(&mut out, &mut bound, dest);
                    spill_placeholder(&mut out, &spill, dest);
                    match target {
                        Some(t) => out.push(format!("    jmp {}", sa_label(t))),
                        None => out.push(format!("    panic({})", unreachable_panic_code(&b.id))),
                    }
                    continue;
                }
                // Calls without a resolvable signature but with args cannot
                // get a typed `@extern` decl (bare `()` mismatches any args:
                // CapabilityMismatch), so they go loud (control preserved).
                if call_sig_loud(sig, args) {
                    unsup.push(format!("{}: CallNoSig", b.id));
                    out.push("    // UNSUPPORTED call-sig: unresolvable callee signature (control preserved)".to_string());
                    bind_placeholder(&mut out, &mut bound, dest);
                    spill_placeholder(&mut out, &spill, dest);
                    match target {
                        Some(t) => out.push(format!("    jmp {}", sa_label(t))),
                        None => out.push(format!("    panic({})", unreachable_panic_code(&b.id))),
                    }
                    continue;
                }
                // Phi loads go before the call (after the loud-arg and
                // signature gates above, so loud paths never create temps).
                for l in &phi_loads {
                    out.push(format!("    {}", l));
                }
                // Slice-literal call args materialize via a preamble (byte
                // buffer + pair) and pass the pair address; other args render
                // inline as before. The preamble shares the block's `mv_idx`
                // counter with the statement arms, so pair temps stay unique.
                let mut a: Vec<String> = vec![];
                let mut pair_bufs: Vec<String> = vec![];
                for arg in args.iter() {
                    let (lines, text, mut b) =
                        crate::render_util::render_call_arg_preamble(arg, &b.id, &mut mv_idx);
                    for l in lines {
                        out.push(format!("    {}", l));
                    }
                    a.push(text);
                    pair_bufs.append(&mut b);
                }
                // Void calls bind nothing (`_x = call @void()` is InvalidSyntax):
                // the unit dest gets an exact `0` marker (unit carries no data).
                let is_void = sig.as_ref().is_some_and(|s| s.ret == "void");
                if rebind {
                    out.push(format!("    call @{}({})", sa_ident(func), a.join(", ")));
                } else if is_void {
                    out.push(format!("    call @{}({})", sa_ident(func), a.join(", ")));
                    if let Some(d) = dest {
                        out.push(format!("    // unit call result (exact: () carries no data)"));
                        out.push(format!("    {} = 0", d));
                        bound.insert(d.clone());
                    }
                } else {
                    out.push(format!("    {} = call @{}({})", dest.as_deref().unwrap_or("_0"), sa_ident(func), a.join(", ")));
                    if let Some(d) = dest {
                        bound.insert(d.clone());
                        // Spilled call dests get their slot setup inline.
                        if let Some(ty) = spill.get(d) {
                            let slot = spill_slot(d);
                            out.push(format!("    {} = alloc 8", slot));
                            out.push(format!("    store {}+0, {} as {}", slot, d, ty));
                        }
                        // Phi def store (non-rebind call dests only).
                        let base = crate::asm::strip_version_suffix(d).to_string();
                        if let Some(ty) = phi_b.get(&base) {
                            out.push(format!("    store {}+0, {} as {}", spill_slot(&base), d, ty));
                        }
                    }
                }
                // Free pair byte buffers right after the call (see
                // render_call_arg_preamble): they are never moved, so no
                // later pass frees them on normal-return paths.
                for b in pair_bufs {
                    out.push(format!("    !{}", b));
                }
                // Phi call-arg temps are plain-param reads: free after the call.
                for t in &phi_frees {
                    out.push(format!("    !{}", t));
                }
                match target {
                    Some(t) => out.push(format!("    jmp {}", sa_label(t))),
                    None => {
                        // Diverging call: panic diverges without ending the
                        // SA function textually (bare `unreachable` would).
                        out.push("    // diverging call".to_string());
                        out.push(format!("    panic({})", unreachable_panic_code(&b.id)));
                    }
                }
            }
            Term::Drop { place, target } => {
                // Conflicted drops carry a phi marker (see version.rs):
                // loud here for now (phi-drop would double-free, see
                // phi.rs), control preserved via the original target.
                if crate::phi::phi_place(place).is_some() || place == "__VERSION_CONFLICT__" {
                    unsup.push(format!("{}: DropConflict", b.id));
                    out.push("    // UNSUPPORTED drop: version conflict at join (multiple reaching defs)".to_string());
                } else if borrow_plan.cleanup.contains(&bi) {
                    // Cleanup duplicate (see borrow_end.rs phase 2): the main
                    // path already freed this place and the unwind path never
                    // runs, so no second `!p` (Referee scans past `return`).
                    out.push(format!("    // cleanup drop {} (omitted: main path owns the release)", place));
                } else {
                    // Borrow-end (see borrow_end.rs): NLL-dead borrowers end
                    // here so `!p` doesn't trap BorrowConflict. Unresolvable
                    // sites stay loud (old shape, honest count).
                    if borrow_plan.loud.contains(&bi) {
                        unsup.push(format!("{}: DropBorrowLive", b.id));
                        out.push(format!("    // UNSUPPORTED drop-borrow-live -> {} (borrow outlives; not provably endable)", place));
                    }
                    if let Some(ends) = borrow_plan.ends.get(&bi) {
                        for r in ends {
                            out.push(format!("    !{}", r));
                        }
                    }
                    out.push(format!("    !{}", place));
                }
                out.push(format!("    jmp {}", sa_label(target)));
            }
            Term::SwitchInt { discr, targets, otherwise } => {
                // Phi rewrite (C2): a conflict discriminant becomes a slot
                // load; unresolvable discriminants keep the loud path.
                let (discr_rw, phi_load, phi_free) =
                    match crate::phi::rewrite_operand(discr, phi_b, &b.id, &mut mv_idx) {
                        Some((n, l, t)) => (Box::new(n), l, t),
                        None => (discr.clone(), None, None),
                    };
                let discr = &discr_rw;
                if let Some(l) = phi_load {
                    out.push(format!("    {}", l));
                }
                // Single-move fan-out: a Move discriminant must be bound once,
                // otherwise every `br ^p` arm would move `p` again. Each arm
                // compares via `eq` + two-target `br` (inline `==` is
                // ForbiddenSyntax); fallthrough lands on per-arm labels.
                // Unresolvable const discriminants go loud (control preserved).
                if const_needs_loud(discr) {
                    let kind = loud_const_kind(discr).unwrap_or("ConstValue");
                    unsup.push(format!("{}: Switch{}", b.id, kind));
                    out.push("    // UNSUPPORTED switch-discr: unresolvable const (control preserved)".to_string());
                    out.push(format!("    jmp {}", sa_label(otherwise)));
                    continue;
                }
                // Field-precision: an unrenderable projection goes
                // loud (control preserved); a renderable field/deref
                // discriminant loads into a temp first (a plain `tmp =
                // place` would bind the root local).
                if crate::render_util::operand_proj_loud(discr) {
                    unsup.push(format!("{}: SwitchFieldProjection", b.id));
                    out.push("    // UNSUPPORTED switch-discr: unrenderable field projection (control preserved)".to_string());
                    out.push(format!("    jmp {}", sa_label(otherwise)));
                    continue;
                }
                let d = if crate::render_util::operand_is_projected(discr) {
                    let (lines, tmp) = crate::render_util::bind_move_operand(discr, &b.id, &mut mv_idx);
                    for l in lines {
                        out.push(format!("    {}", l));
                    }
                    tmp
                } else {
                    match discr.as_ref() {
                        Operand::Move { place , .. } => {
                            let tmp = format!("_sw_{}", b.id);
                            out.push(format!("    {} = {}", tmp, place));
                            tmp
                        }
                        _ => render_operand(discr),
                    }
                };
                for (i, (v, t)) in targets.iter().enumerate() {
                    // Else-edge of the previous arm lands here (col-0 label),
                    // so the chain falls through arm by arm.
                    if i > 0 {
                        out.push(format!("{}:", sa_label(&format!("sw_{}_{}", b.id, i - 1))));
                    }
                    let e = format!("_sw_{}_eq_{}", b.id, i);
                    out.push(format!("    {} = eq {}, {}", e, d, v));
                    if i + 1 < targets.len() {
                        out.push(format!("    br {} -> {}, {}", e, sa_label(t), sa_label(&format!("sw_{}_{}", b.id, i))));
                    } else {
                        out.push(format!("    br {} -> {}, {}", e, sa_label(t), sa_label(otherwise)));
                    }
                }
                // No-arm switch (empty targets): straight to otherwise.
                if targets.is_empty() {
                    out.push(format!("    jmp {}", sa_label(otherwise)));
                }
                // Phi discriminant temp is read by every `eq`: free it
                // before the last branch (a line may not follow a
                // terminator, so it goes before, not after, the last br).
                if let Some(t) = phi_free {
                    if let Some(last) = out.last() {
                        if last.trim_start().starts_with("br ")
                            || last.trim_start().starts_with("jmp ")
                        {
                            let pos = out.len() - 1;
                            out.insert(pos, format!("    !{}", t));
                        }
                    }
                }
            }
            Term::Assert { cond, target, msg, expected } => {
                // Phi rewrite (C2): a conflict condition becomes a slot load.
                let (cond_rw, phi_load, phi_free) =
                    match crate::phi::rewrite_operand(cond, phi_b, &b.id, &mut mv_idx) {
                        Some((n, l, t)) => (Box::new(n), l, t),
                        None => (cond.clone(), None, None),
                    };
                let cond = &cond_rw;
                if let Some(l) = phi_load {
                    out.push(format!("    {}", l));
                }
                // Overflow asserts are folded: the accompanying
                // `*WithOverflow` already lowered to `sa_std/num.sai`'s
                // checked helper, which traps on overflow. Comparing the flag
                // here would test the VALUE register (the pair is collapsed),
                // i.e. nonsense. Counted loudly so the folding stays visible.
                let msg_text = msg.as_deref().unwrap_or("");
                if is_overflow_assert(msg_text) {
                    unsup.push(format!("{}: OverflowAssertFolded", b.id));
                    out.push(format!(
                        "    // overflow assert folded: {} traps inside sa_std/num.sai ({} )",
                        sa_label(target),
                        flat_comment(msg_text)
                    ));
                    // A folded phi temp is never used: release it before the jump.
                    if let Some(t) = phi_free {
                        out.push(format!("    !{}", t));
                    }
                    out.push(format!("    jmp {}", sa_label(target)));
                    continue;
                }
                // `assert cond` is not an SA instruction: compare against the
                // expected bit, branch to target, else numeric panic (the msg
                // rides as a comment; codes are 1500+bb deterministic).
                // Unresolvable const conditions go loud (control preserved).
                if const_needs_loud(cond) {
                    let kind = loud_const_kind(cond).unwrap_or("ConstValue");
                    unsup.push(format!("{}: Assert{}", b.id, kind));
                    out.push("    // UNSUPPORTED assert-cond: unresolvable const (control preserved)".to_string());
                    out.push(format!("    jmp {}", sa_label(target)));
                    continue;
                }
                // Field-precision: same rule as the switch
                // discriminant above (loud gate, then field load).
                if crate::render_util::operand_proj_loud(cond) {
                    unsup.push(format!("{}: AssertFieldProjection", b.id));
                    out.push("    // UNSUPPORTED assert-cond: unrenderable field projection (control preserved)".to_string());
                    out.push(format!("    jmp {}", sa_label(target)));
                    continue;
                }
                let c = if crate::render_util::operand_is_projected(cond) {
                    let (lines, tmp) = crate::render_util::bind_move_operand(cond, &b.id, &mut mv_idx);
                    for l in lines {
                        out.push(format!("    {}", l));
                    }
                    tmp
                } else {
                    match cond.as_ref() {
                        Operand::Move { place , .. } => {
                            let tmp = format!("_as_{}", b.id);
                            out.push(format!("    {} = {}", tmp, place));
                            tmp
                        }
                        _ => render_operand(cond),
                    }
                };
                let e = format!("_as_{}_eq", b.id);
                out.push(format!("    {} = eq {}, {}", e, c, if expected.unwrap_or(true) { 1 } else { 0 }));
                // Phi condition temp is read by the `eq`: release it before
                // the branch (it must not follow the terminator).
                if let Some(t) = phi_free {
                    out.push(format!("    !{}", t));
                }
                out.push(format!("    br {} -> {}, {}", e, sa_label(target), sa_label(&format!("as_{}_fail", b.id))));
                out.push(format!("{}:", sa_label(&format!("as_{}_fail", b.id))));
                match msg {
                    Some(m) => {
                        out.push(format!("    // assert failed: {}", flat_comment(m)));
                        out.push(format!("    panic({})", assert_panic_code(&b.id)));
                    }
                    None => {
                        out.push("    // assert failed".to_string());
                        out.push(format!("    panic({})", assert_panic_code(&b.id)));
                    }
                }
            }
            Term::Unreachable => {
                out.push("    // MIR unreachable (diverging path)".to_string());
                out.push(format!("    panic({})", unreachable_panic_code(&b.id)));
            }
            Term::InlineAsm { text, template, options, modifiers, inout, outs, ins, target } => {
                let t = template.as_deref();
                let o = options.as_deref();
                match asm_mov_copy(t, o, *modifiers, *inout, outs, ins) {
                    Some((dest, src)) => {
                        if bound.contains(&dest) {
                            unsup.push(format!("{}:{} Rebind", b.id, dest));
                            out.push(format!("    // UNSUPPORTED rebind -> {} (already bound; keeping first value)", dest));
                        } else {
                            out.push("    // inline-asm mov (exact reg copy)".to_string());
                            out.push(format!("    {} = {}", dest, src));
                            let base = crate::asm::strip_version_suffix(&dest).to_string();
                            if let Some(ty) = phi_b.get(&base) {
                                out.push(format!("    store {}+0, {} as {}", spill_slot(&base), dest, ty));
                            }
                            bound.insert(dest);
                        }
                    }
                    None => match asm_inout_passthrough(t, o, *modifiers, *inout, outs, ins) {
                        Some((None, None)) => {
                            out.push("    // inline-asm inout passthrough (value-stable escape, sla-117)".to_string());
                        }
                        Some((Some(dest), Some(src))) => {
                            if bound.contains(&dest) {
                                unsup.push(format!("{}:{} Rebind", b.id, dest));
                                out.push(format!("    // UNSUPPORTED rebind -> {} (already bound; keeping first value)", dest));
                            } else {
                                out.push("    // inline-asm inout passthrough (value-stable escape)".to_string());
                                out.push(format!("    {} = {}", dest, src));
                                let base = crate::asm::strip_version_suffix(&dest).to_string();
                                if let Some(ty) = phi_b.get(&base) {
                                    out.push(format!("    store {}+0, {} as {}", spill_slot(&base), dest, ty));
                                }
                                bound.insert(dest);
                            }
                        }
                        _ => {
                            unsup.push(format!("{}: InlineAsm", b.id));
                            out.push(format!("    // UNSUPPORTED inline-asm: {} (no SA equivalent; extern/intrinsic TBD)", flat_comment(text)));
                            // Single plain-local out gets a placeholder so
                            // downstream uses stay parseable.
                            if outs.len() == 1 && is_plain_local(outs[0].trim()) && !bound.contains(&outs[0]) {
                                out.push(format!("    {} = 0", outs[0].trim()));
                                bound.insert(outs[0].trim().to_string());
                                // Phi def store for the placeholder value.
                                let base = crate::asm::strip_version_suffix(outs[0].trim()).to_string();
                                if let Some(ty) = phi_b.get(&base) {
                                    out.push(format!("    store {}+0, {} as {}", spill_slot(&base), outs[0].trim(), ty));
                                }
                            }
                        }
                    },
                }
                // Explicit fallthrough: RPO reordering breaks MIR adjacency.
                // No target (naked/noreturn): panic diverges without ending
                // the SA function textually (bare `unreachable` would).
                match target {
                    Some(t) => out.push(format!("    jmp {}", sa_label(t))),
                    None => {
                        out.push("    // noreturn/naked asm (diverging path)".to_string());
                        out.push(format!("    panic({})", unreachable_panic_code(&b.id)));
                    }
                }
            }
            Term::Unsupported { text } => {
                unsup.push(format!("{}: UnsupportedTerm", b.id));
                out.push(format!("    // UNSUPPORTED terminator: {}", text));
            }
        }
    }
    if let Some(prev) = open_block {
        let e = out.len();
        block_ranges.push((prev, block_start[prev], e));
    }
    // Drop glue (see drop.rs): exit-anchored releases over emitted lines.
    // Insertion runs over original block indices (dom_sets keying), from
    // last to first so earlier indices stay valid.
    {
        let n = f.blocks.len();
        let mut lines_by_orig: Vec<Vec<String>> = vec![vec![]; n];
        let mut range_by_orig: Vec<(usize, usize)> = vec![(0, 0); n];
        for (bi, s, e) in &block_ranges {
            lines_by_orig[*bi] = out[*s..*e].to_vec();
            range_by_orig[*bi] = (*s, *e);
        }
        // Phase-5 Call-block ends (borrow_end phase 5): they go after
        // the call and before the block's jmp, so the call args are
        // still live. Inject into the working copy BEFORE drop.rs runs
        // so exit/stale analysis sees them as ordinary frees (a borrower
        // ended here must not be freed again at an exit), and into the
        // final splice at the same position below. Blocks whose last
        // line is not a jmp (diverging calls) take no end.
        for (bi, ends) in &borrow_plan.call_ends {
            let lines = &mut lines_by_orig[*bi];
            let is_jmp = lines.last().is_some_and(|l| l.trim_start().starts_with("jmp "));
            if is_jmp {
                let pos = lines.len() - 1;
                let mut k = 0usize;
                for e in ends {
                    for l in join_end_lines(e, &spill) {
                        lines.insert(pos + k, l);
                        k += 1;
                    }
                }
            }
        }
        let mut frees = crate::drop::exit_frees(&lines_by_orig, &doms, f.params.len());
        let mut tfrees = crate::drop::temp_frees(&lines_by_orig);
        // Block-local plain-move frees (see drop::move_frees): merged
        // into the temp set so the same one-direction cross-filter with
        // exit_frees applies, and deduped against temp_frees (an
        // `_as_*`/`_sw_*` base temp is a plain move too, but move_frees
        // already excludes that class; the dedup is belt-and-braces
        // against any future overlap producing a double free).
        {
            let already: std::collections::BTreeSet<String> =
                tfrees.values().flatten().cloned().collect();
            for (bi, v) in crate::drop::move_frees(&lines_by_orig) {
                let e = tfrees.entry(bi).or_default();
                for r in v {
                    if !already.contains(&r) && !e.contains(&r) {
                        e.push(r);
                    }
                }
            }
            // Call dests consumed exactly once as a call argument (see
            // drop::call_dest_frees): merged the same way, so the
            // one-direction cross-filter against exit_frees below
            // applies to them too (never freed twice).
            let already2: std::collections::BTreeSet<String> =
                tfrees.values().flatten().cloned().collect();
            for (bi, v) in crate::drop::call_dest_frees(&lines_by_orig) {
                let e = tfrees.entry(bi).or_default();
                for r in v {
                    if !already2.contains(&r) && !e.contains(&r) {
                        e.push(r);
                    }
                }
            }
        }
        // A temp already released at an exit keeps that release (the exit list
        // is dominance-verified, the block-local one is heuristic). Filter in
        // ONE direction only: cross-filtering both ways would drop a register
        // that appears in both lists from both.
        let temp_freed: std::collections::BTreeSet<String> =
            tfrees.values().flatten().cloned().collect();
        for v in frees.values_mut() {
            v.retain(|r| !temp_freed.contains(r));
        }
        let exit_freed: std::collections::BTreeSet<String> =
            frees.values().flatten().cloned().collect();
        for v in tfrees.values_mut() {
            v.retain(|r| !exit_freed.contains(r));
        }
        // Spill-slot pairing: a spilled base freed anywhere leaves its
        // slot allocated forever unless the slot is freed alongside it
        // (30: _4 freed by move_frees at bb4's end, _4_spill leaked).
        // The slot is always bound at the base's free point (same block
        // for block-local frees; the def block dominates the exit for
        // exit frees). Skip when the slot is already freed (phase-5
        // ends in lines_by_orig) or listed for the same block. A slot
        // that is itself an exit candidate is dropped from exits
        // dominated by the block-end free (every path to such an exit
        // passes the block-end free, so the exit free would double).
        {
            let freed_in_lines: std::collections::BTreeSet<String> = lines_by_orig
                .iter()
                .flat_map(|ls| ls.iter())
                .filter_map(|l| {
                    let t = l.trim_start();
                    t.strip_prefix('!').map(|r| r.trim().to_string())
                })
                .collect();
            // (block, base, from_tfrees): collected first (borrow rules).
            // uam-tail: successor graph over lines_by_orig, for the
            // slot-later-use guard below (188: a spill slot reloaded in
            // a successor block must not be freed with its base).
            let mut label_to_bi: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
            for (idx, ls) in lines_by_orig.iter().enumerate() {
                if let Some(first) = ls.first() {
                    let ft = first.trim();
                    if ft.ends_with(":") {
                        label_to_bi.insert(ft.trim_end_matches(":").to_string(), idx);
                    }
                }
            }
            let mut succ: Vec<std::collections::BTreeSet<usize>> = vec![std::collections::BTreeSet::new(); lines_by_orig.len()];
            for (idx, ls) in lines_by_orig.iter().enumerate() {
                for l in ls {
                    let lt = l.trim_start();
                    if let Some(rest) = lt.strip_prefix("jmp ") {
                        if let Some(tb) = label_to_bi.get(rest.trim()) {
                            succ[idx].insert(*tb);
                        }
                    } else if let Some(rest) = lt.strip_prefix("br ") {
                        if let Some(arrow) = rest.find("->") {
                            for tgt in rest[arrow + 2..].split(",") {
                                if let Some(tb) = label_to_bi.get(tgt.trim()) {
                                    succ[idx].insert(*tb);
                                }
                            }
                        }
                    }
                }
            }
            let mut pairs: Vec<(usize, String, bool)> = vec![];
            for (bi, v) in frees.iter() {
                for r in v {
                    pairs.push((*bi, r.clone(), false));
                }
            }
            for (bi, v) in tfrees.iter() {
                for r in v {
                    pairs.push((*bi, r.clone(), true));
                }
            }
            for (bi, base, from_t) in pairs {
                if !spill.contains_key(&base) {
                    continue;
                }
                let slot = format!("{}_spill", base);
                if freed_in_lines.contains(&slot) {
                    continue;
                }
                // uam-tail: skip pairing when a reachable successor
                // block still mentions the slot (its reload is after
                // this free point). The slot is then left to
                // exit_frees; freeing it here would trap UseAfterMove
                // at the reload (188: _12_spill reloaded in bb6 after
                // _12 was move-freed at the end of bb5).
                {
                    let mut seen: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
                    let mut stack: Vec<usize> = vec![bi];
                    while let Some(cur) = stack.pop() {
                        if !seen.insert(cur) {
                            continue;
                        }
                        for s in succ.get(cur).cloned().unwrap_or_default() {
                            stack.push(s);
                        }
                    }
                    let mut later_use = false;
                    for rb in &seen {
                        if *rb == bi {
                            continue;
                        }
                        if lines_by_orig[*rb].iter().any(|l| l.contains(&slot)) {
                            later_use = true;
                            break;
                        }
                    }
                    if later_use {
                        continue;
                    }
                }
                let listed_here = if from_t {
                    tfrees.get(&bi).is_some_and(|v| v.contains(&slot))
                } else {
                    frees.get(&bi).is_some_and(|v| v.contains(&slot))
                };
                if listed_here {
                    continue;
                }
                if from_t {
                    tfrees.entry(bi).or_default().push(slot.clone());
                    // Drop the slot from exits dominated by this block.
                    for (e, v) in frees.iter_mut() {
                        if doms[*e].contains(&bi) {
                            v.retain(|r| r != &slot);
                        }
                    }
                } else {
                    frees.entry(bi).or_default().push(slot);
                }
            }
        }
        let mut mvfrees = crate::drop::mv_frees(&lines_by_orig);
        // A register already released at an exit or as a compare temp keeps
        // that release: re-releasing it block-locally would double-free.
        // One direction only, as above: mv_frees never feeds back into
        // frees/tfrees, so nothing can be dropped from both lists.
        let released: std::collections::BTreeSet<String> = frees
            .values()
            .flatten()
            .chain(tfrees.values().flatten())
            .cloned()
            .collect();
        for v in mvfrees.values_mut() {
            v.retain(|(_, r)| !released.contains(r));
        }
        mvfrees.retain(|_, v| !v.is_empty());
        // Combined insertions keyed by flat position: exit/temp frees go
        // before the block terminator, mv frees go right after the temp's
        // last use. One splice per position keeps order, and positions are
        // applied from last to first so earlier ones stay valid.
        let mut inserts: std::collections::BTreeMap<usize, Vec<String>> =
            std::collections::BTreeMap::new();
        for bi in 0..n {
            // Phase-5 Call-block ends land at the same pre-terminator
            // position as exit/temp frees; borrower ends precede any
            // source free at a shared position.
            if let Some(v) = borrow_plan.call_ends.get(&bi) {
                let (s, e) = range_by_orig[bi];
                let last_is_jmp = e > s && out[e - 1].trim_start().starts_with("jmp ");
                if last_is_jmp && !v.is_empty() {
                    let pos = e.saturating_sub(1).max(s);
                    for e2 in v {
                        for l in join_end_lines(e2, &spill) {
                            inserts.entry(pos).or_default().push(l);
                        }
                    }
                }
            }
            let mut fs: Vec<String> = vec![];
            if let Some(v) = frees.get(&bi) {
                fs.extend(v.iter().cloned());
            }
            if let Some(v) = tfrees.get(&bi) {
                fs.extend(v.iter().cloned());
            }
            if !fs.is_empty() {
                let (s, e) = range_by_orig[bi];
                // The jump/return is the block's last line: insert before it (a
                // label-less line may not follow a terminator).
                let pos = e.saturating_sub(1).max(s);
                for r in fs {
                    inserts.entry(pos).or_default().push(format!("    !{}", r));
                }
            }
            if let Some(v) = mvfrees.get(&bi) {
                let (s, _) = range_by_orig[bi];
                let mut by_line: std::collections::BTreeMap<usize, Vec<String>> =
                    std::collections::BTreeMap::new();
                for (li, r) in v {
                    by_line.entry(*li).or_default().push(r.clone());
                }
                for (li, rs) in by_line {
                    let pos = s + li + 1;
                    for r in rs {
                        inserts.entry(pos).or_default().push(format!("    !{}", r));
                    }
                }
            }
        }
        for (pos, lines) in inserts.iter().rev() {
            out.splice(*pos..*pos, lines.iter().cloned());
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

pub fn collect_externs(mir: &MirFile) -> std::collections::BTreeMap<String, Option<(Vec<String>, String)>> {
    // sla convention (cf. sa_plugin_sla emitExternDecl): every called-but-
    // undefined symbol gets an `@extern` decl so the module is closed.
    // Signatures are per first-seen call site (monomorphic MIR gives one sig
    // per DefId; name truncation collisions keep the first, documented).
    // Signatures are unknown at MIR level; map them to sci/sa_std per STD_MAP.md.
    let mut set = std::collections::BTreeMap::new();
    for f in &mir.functions {
        for b in &f.blocks {
            for st in &b.statements {
                if let Stmt::Assign { rvalue: Rvalue::Call { func, sig, .. }, .. } = st {
                    set.entry(sa_ident(func)).or_insert_with(|| {
                        sig.as_ref().map(|s| (s.params.clone(), s.ret.clone()))
                    });
                }
            }
            if let Term::Call { func, sig, .. } = &b.terminator {
                set.entry(sa_ident(func)).or_insert_with(|| {
                    sig.as_ref().map(|s| (s.params.clone(), s.ret.clone()))
                });
            }
        }
    }
    set
}

/// `@extern` decl text: typed params when known, bare `()` otherwise
/// (bare decls only match zero-arg calls; the rest go loud at use sites).
pub fn extern_decl(name: &str, sig: &Option<(Vec<String>, String)>) -> String {
    match sig {
        Some((params, ret)) => {
            let ps: Vec<String> = params
                .iter()
                .enumerate()
                .map(|(i, t)| format!("_a{}: {}", i, t))
                .collect();
            format!("@extern {}({}) -> {}", name, ps.join(", "), ret)
        }
        None => format!("@extern {}()", name),
    }
}

/// Assemble the .sa text for an already-versioned MirFile: `@extern`
/// closure decls plus one body per function. Shared by `lower` (one
/// input file) and `link` (several merged files) so both emit the
/// same header/extern/body shape; returns (sa_text, unsupported).
pub fn render_sa(mir: &MirFile) -> (String, Vec<String>) {
    let mut unsup = vec![];
    // Bodies joined by "\n"; each body already ends with one trailing newline.
    // sla convention: `@extern` decls for all called-but-undefined symbols.
    let mut sa_compat = String::from("@import \"sa_std/io/print.sai\"\n@import \"sa_std/num.sai\"\n\n");
    let mut exts = collect_externs(mir);
    let layouts = crate::place_store::build_adt_layout_index(&mir.functions);
    let bodies: Vec<String> = mir
        .functions
        .iter()
        .map(|f| lower_function_with_layouts(f, &mut unsup, &layouts))
        .collect();
    if bodies.iter().any(|b| b.contains("sa_mem_set")) {
        // Repeat lowering emits `call @sa_mem_set` directly (not via Rvalue::Call).
        // Signature mirrors `sci/sa_std/core/mem.sa` (`&dst: ptr, val: u8, count: u64 -> void`).
        exts.insert(
            "sa_mem_set".to_string(),
            Some((vec!["ptr".to_string(), "u8".to_string(), "u64".to_string()], "void".to_string())),
        );
    }
    if bodies.iter().any(|b| b.contains("sa_thread_local_slot")) {
        // ThreadLocal lowering emits `call @sa_thread_local_slot` directly
        // (registry lives in sci/sa_std/thread_local.sai: `(key: u64) -> ptr`).
        exts.insert(
            "sa_thread_local_slot".to_string(),
            Some((vec!["u64".to_string()], "ptr".to_string())),
        );
    }
    if !exts.is_empty() {
        sa_compat += "// MIR callees (map to sci/sa_std per STD_MAP.md):\n";
        for (e, sig) in &exts {
            sa_compat += &extern_decl(e, sig);
            sa_compat += "\n";
        }
        sa_compat += "\n";
    }
    sa_compat += &bodies.join("\n");
    (sa_compat, unsup)
}

pub fn cmd_lower(args: &[String]) -> ExitCode {
    let (mut input, mut out, mut strict) = (None, None, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => { i += 1; out = args.get(i).cloned(); }
            "--strict" => strict = true,
            s if !s.starts_with("--") && input.is_none() => input = Some(s.to_string()),
            _ => {}
        }
        i += 1;
    }
    let (input, out) = match (input, out) {
        (Some(a), Some(b)) => (a, b),
        _ => { eprintln!("usage: mir2sa lower <mir.json> --out <out.sa> [--strict]"); return ExitCode::from(2); }
    };
    let text = match std::fs::read_to_string(&input) {
        Ok(t) => t,
        Err(e) => { eprintln!("read {}: {}", input, e); return ExitCode::from(2); }
    };
    let mir: MirFile = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(e) => { eprintln!("bad mir.json: {}", e); return ExitCode::from(2); }
    };
    // SSA versioning (see version.rs): same transform coverage applies.
    let mir = MirFile {
        source: mir.source.clone(),
        crate_name: mir.crate_name.clone(),
        functions: mir.functions.iter().map(crate::version::version_function).collect(),
    };
    let (sa_compat, unsup) = render_sa(&mir);

    if std::fs::write(&out, &sa_compat).is_err() {
        eprintln!("write {}", out);
        return ExitCode::from(2);
    }
    println!("wrote {} UNSUPPORTED={}", out, unsup.len());
    for u in &unsup {
        eprintln!("  UNSUPPORTED: {}", u);
    }
    if strict && !unsup.is_empty() {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

