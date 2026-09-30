//! mir2sa::lower.
use std::process::ExitCode;

use crate::asm::{asm_inout_passthrough, asm_mov_copy, is_plain_local};
use crate::mir::*;
use crate::render::*;
use crate::render_util::{assert_panic_code, assign_loud_const, build_constmap, call_sig_loud, const_needs_loud, flat_comment, render_call_arg, render_operand, sa_ident, sa_label, unreachable_panic_code};
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

pub fn lower_function(f: &Function, unsup: &mut Vec<String>) -> String {    // Typed header: MIR arg locals `_1..` bind here (bare `@f()` would leave
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
    let doms = crate::order::dom_sets(&f.blocks);
    let borrow_plan = crate::borrow_end::plan_drops(&f.blocks, &doms);
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
    // Spill map for multi-use shared values (reload slots instead of
    // moving shared temps twice). Built over MIR statements once.
    let spill = build_spill(&f.blocks);
    for bi in order {
        let b = &f.blocks[bi];
        let start = out.len();
        out.push(format!("{}:", sa_label(&b.id)));
        // Per-block bound set: repeats go loud and keep the first value
        // (skip the line). Exclusive-branch joins stay legal (never seeded).
        let mut bound = seeds[bi].clone();
        let mut mv_idx = 0usize;
        for st in &b.statements {
            match st {
                Stmt::Assign { dest, dest_place, rvalue } => {
                    // Trailing `//` is ForbiddenSyntax: the Assign arm never
                    // appends provenance comments (they live in mir.json).
                    if bound.contains(dest) {
                        unsup.push(format!("{}:{} Rebind", b.id, dest));
                        out.push(format!("    // UNSUPPORTED rebind -> {} (already bound; keeping first value)", dest));
                        continue;
                    }
                    // Unresolvable const values go loud (never raw Debug).
                    let before = unsup.len();
                    if let Some(why) = assign_loud_const(rvalue) {
                        unsup.push(format!("{}:{} ConstValue", b.id, dest));
                        out.push(format!("    // UNSUPPORTED const-value -> {}: {}", dest, flat_comment(&why)));
                    } else {
                        let line = render_rvalue(rvalue, dest, dest_place.as_deref(), unsup, &b.id, &mut mv_idx, &cmap, &spill);
                        for l in line.split('\n') {
                            out.push(format!("    {}", l));
                        }
                    }
                    if unsup.len() > before {
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
                }
                Stmt::StorageLive { local } => out.push(format!("    // StorageLive {}", local)),
                Stmt::StorageDead { local } => out.push(format!("    // StorageDead {}", local)),
                Stmt::Nop { text } => out.push(format!("    // nop: {}", text)),
                Stmt::SetDisc { place, variant, .. } => {
                    // p_layout v1: enum tag lives at offset 0 (sla enum_tag_offset=0,
                    // payload at 8). Driver reports the base local already; the
                    // full place (`(*_9)`) is kept as a comment for provenance.
                    out.push("    // set-discriminant (enum tag)".to_string());
                    out.push(format!("    store {}+0, {} as i64", place, variant));
                }
                Stmt::UnsupportedStmt { text } => {
                    unsup.push(format!("{}: UnsupportedStmt", b.id));
                    out.push(format!("    // UNSUPPORTED stmt: {}", text));
                }
            }
        }
        match &b.terminator {
            Term::Goto { target } => out.push(format!("    jmp {}", sa_label(target))),
            Term::Return => out.push("    return 0".to_string()),
            Term::Resume => {
                out.push("    // MIR Resume (cleanup path)".to_string());
                out.push("    panic(\"unwind-resume\")".to_string());
            }
            Term::Call { func, func_raw, args, dest, target, sig } => {
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
                if args.iter().any(const_needs_loud) {
                    unsup.push(format!("{}: CallConstValue", b.id));
                    out.push("    // UNSUPPORTED call-args: unresolvable const (control preserved)".to_string());
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
                let a: Vec<String> = args.iter().map(render_call_arg).collect();
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
                    }
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
                // Conflicted drops carry a marker (see version.rs): loud here,
                // control preserved via the original target.
                if place == "__VERSION_CONFLICT__" {
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
                // Single-move fan-out: a Move discriminant must be bound once,
                // otherwise every `br ^p` arm would move `p` again. Each arm
                // compares via `eq` + two-target `br` (inline `==` is
                // ForbiddenSyntax); fallthrough lands on per-arm labels.
                // Unresolvable const discriminants go loud (control preserved).
                if const_needs_loud(discr) {
                    unsup.push(format!("{}: SwitchConstValue", b.id));
                    out.push("    // UNSUPPORTED switch-discr: unresolvable const (control preserved)".to_string());
                    out.push(format!("    jmp {}", sa_label(otherwise)));
                    continue;
                }
                let d = match discr.as_ref() {
                    Operand::Move { place } => {
                        let tmp = format!("_sw_{}", b.id);
                        out.push(format!("    {} = {}", tmp, place));
                        tmp
                    }
                    _ => render_operand(discr),
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
            }
            Term::Assert { cond, target, msg, expected } => {
                // `assert cond` is not an SA instruction: compare against the
                // expected bit, branch to target, else numeric panic (the msg
                // rides as a comment; codes are 1500+bb deterministic).
                // Unresolvable const conditions go loud (control preserved).
                if const_needs_loud(cond) {
                    unsup.push(format!("{}: AssertConstValue", b.id));
                    out.push("    // UNSUPPORTED assert-cond: unresolvable const (control preserved)".to_string());
                    out.push(format!("    jmp {}", sa_label(target)));
                    continue;
                }
                let c = match cond.as_ref() {
                    Operand::Move { place } => {
                        let tmp = format!("_as_{}", b.id);
                        out.push(format!("    {} = {}", tmp, place));
                        tmp
                    }
                    _ => render_operand(cond),
                };
                let e = format!("_as_{}_eq", b.id);
                out.push(format!("    {} = eq {}, {}", e, c, if expected.unwrap_or(true) { 1 } else { 0 }));
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
        block_ranges.push((bi, start, out.len()));
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
        let frees = crate::drop::exit_frees(&lines_by_orig, &doms, f.params.len());
        let mut order_desc: Vec<usize> = (0..n).collect();
        order_desc.sort_by_key(|b| std::cmp::Reverse(range_by_orig[*b].0));
        for bi in order_desc {
            if let Some(fs) = frees.get(&bi) {
                let (s, e) = range_by_orig[bi];
                // Return line is last in exit blocks; insert before it.
                let pos = e.saturating_sub(1).max(s);
                for (k, r) in fs.iter().enumerate() {
                    out.insert(pos + k, format!("    !{}", r));
                }
            }
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
        functions: mir.functions.iter().map(crate::version::version_function).collect(),
    };
    let mut unsup = vec![];
    // Bodies joined by "\n"; each body already ends with one trailing newline.
    // sla convention: `@extern` decls for all called-but-undefined symbols.
    let mut sa_compat = String::from("@import \"sa_std/io/print.sai\"\n\n");
    let mut exts = collect_externs(&mir);
    let bodies: Vec<String> = mir.functions.iter().map(|f| lower_function(f, &mut unsup)).collect();
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

