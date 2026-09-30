//! mir2sa: MIR -> SA backend in pure Rust.
//!
//! Subcommands (1:1 replacements of the retired Python prototypes):
//!   mir2sa parse <mir-text> --fn <name> --out <mir.json>   # real `-Zunpretty=mir` -> mir.json
//!   mir2sa lower <mir.json> --out <out.sa> [--strict]      # mir.json -> .sa
//!
//! Mapping (identical to the tutorial + retired prototype):
//!   Operand::Move(p)  -> p (plain `=` already moves; `^` only in call args)
//!   Operand::Copy(p) -> p
//!   Rvalue::Ref       -> &p        Terminator::Drop(p) -> !p

mod asm;
mod borrow_end;
mod const_util;
mod drop;
mod layout;
mod lower;
mod mir;
mod order;
mod parse;
mod render;
mod render_util;
mod spill;
mod version;

use std::process::ExitCode;

use crate::asm::*;
use crate::const_util::*;
use crate::layout::*;
use crate::lower::*;
use crate::mir::*;
use crate::parse::*;
use crate::render_util::*;
use crate::version::version_function;
fn cmd_coverage(args: &[String]) -> ExitCode {
    let input = args.iter().find(|a| !a.starts_with("--")).cloned();
    let input = match input {
        Some(a) => a,
        None => { eprintln!("usage: mir2sa coverage <mir.json>"); return ExitCode::from(2); }
    };
    let text = match std::fs::read_to_string(&input) {
        Ok(t) => t,
        Err(e) => { eprintln!("read {}: {}", input, e); return ExitCode::from(2); }
    };
    let mir: MirFile = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(e) => { eprintln!("bad mir.json: {}", e); return ExitCode::from(2); }
    };
    let (mut tot_stmts, mut tot_terms, mut tot_unsup) = (0usize, 0usize, 0usize);
    // SSA versioning (see version.rs): multi-def locals renamed, join
    // conflicts marked — lower and coverage share the identical input.
    let mir = MirFile {
        source: mir.source.clone(),
        functions: mir.functions.iter().map(version_function).collect(),
    };
    for f in &mir.functions {
        let mut kinds = std::collections::BTreeMap::<String, usize>::new();
        let mut unsup = vec![];
        // Unrepresentable signature parts (128-bit ints) fall back to `ptr`
        // in the header: parse-clean but counted loud once per function.
        if !f.sig_ok {
            tot_unsup += 1;
            unsup.push(format!("FnSig"));
        }
        // Mirror lower()'s bound seeding (dominator defs + params).
        let seeds = crate::order::dom_seeds(&f.blocks, f.params.len());
        // Mirror lower()'s borrow-end plan (same versioned input).
        let bend = crate::borrow_end::plan_drops(&f.blocks, &crate::order::dom_sets(&f.blocks));
        for (bi, b) in f.blocks.iter().enumerate() {
            // NOTE: seeding above runs once; keep it outside the loop.
            let mut bound = seeds[bi].clone();
            for s in &b.statements {
                tot_stmts += 1;
                *kinds.entry(stmt_kind(s)).or_insert(0) += 1;
                match s {
                    Stmt::Assign { dest, dest_place, rvalue } => {
                        if bound.contains(dest) {
                            tot_unsup += 1;
                            unsup.push(format!("{}:{} Rebind", b.id, dest));
                            continue;
                        }
                        bound.insert(dest.clone());
                        // Mirror lower(): only count what lower() cannot emit.
                        // Unresolvable const values go loud first (never raw).
                        // Calls without signatures mirror lower()'s CallNoSig.
                        let fails = if assign_loud_const(rvalue).is_some() {
                            true
                        } else if let Rvalue::Call { args, sig, .. } = rvalue {
                            call_sig_loud(sig, args)
                        } else {
                            match rvalue {
                                Rvalue::Unsupported { .. } => true,
                                // ThreadLocal lowers to sa_thread_local_slot (sci registry).
                                Rvalue::BinOp { op, .. } => binop_mnemonic(op).is_none(),
                                Rvalue::UnOp { op, .. } => unop_needs_loud(op),
                                Rvalue::Cast { castkind, src_ty, ty, .. } => {
                                    match (castkind.as_deref(), src_ty.as_deref()) {
                                        (Some(k), Some(s)) => lower_cast(k, s, &cast_dst_short(ty)).is_none(),
                                        _ => true,
                                    }
                                }
                                Rvalue::Repeat { op, len } => match op.as_ref() {
                                    Operand::Const { value, .. } => repeat_plan(value, len).is_none(),
                                    _ => true,
                                },
                                Rvalue::Aggregate { elems, layout } if elems.len() > 1 => match dest_place {
                                    Some(p) => {
                                        lower_array_init(&b.id, p, elems).is_none()
                                            && lower_adt_init(&b.id, dest, p, elems, layout.as_ref()).is_none()
                                    }
                                    None => true,
                                },
                                _ => false,
                            }
                        };
                        if fails {
                            tot_unsup += 1;
                            unsup.push(format!("{}:{} {}({})", b.id, dest, rvalue_kind(rvalue),
                                match rvalue {
                                    Rvalue::Unsupported { text } => text.chars().take(60).collect::<String>(),
                                    _ => String::new(),
                                }));
                        }
                    }
                    Stmt::SetDisc { .. } => {
                        // Lowered as `store base+0, variant as i64` (sla enum tag).
                    }
                    Stmt::UnsupportedStmt { text } => {
                        tot_unsup += 1;
                        unsup.push(format!("{}: UnsupportedStmt({})", b.id, text.chars().take(60).collect::<String>()));
                    }
                    _ => {}
                }
            }
            tot_terms += 1;
            *kinds.entry(format!("T/{}", term_kind(&b.terminator))).or_insert(0) += 1;
            if let Term::Unsupported { text } = &b.terminator {
                tot_unsup += 1;
                unsup.push(format!("{}: T/Unsupported({})", b.id, text.chars().take(60).collect::<String>()));
            }
            // Version-conflicted returns lower() cannot express (join over
            // two reaching versions): counted like lower()'s ReturnConflict.
            if let Term::Return { ret: Some(r) } = &b.terminator {
                if r == "__VERSION_CONFLICT__" {
                    tot_unsup += 1;
                    unsup.push(format!("{}: T/ReturnConflict", b.id));
                }
            }
            // Conflict-marked drops (see version.rs) go loud like lower().
            // Borrow-live drops lower() cannot provably end go loud too.
            // Cleanup duplicates are deduped by lower() (borrow_end phase 2)
            // and are NOT a gap, so they are not counted.
            if let Term::Drop { place, .. } = &b.terminator {
                if place == "__VERSION_CONFLICT__" {
                    tot_unsup += 1;
                    unsup.push(format!("{}: T/DropConflict", b.id));
                } else if !bend.cleanup.contains(&bi) && bend.loud.contains(&bi) {
                    tot_unsup += 1;
                    unsup.push(format!("{}: T/DropBorrowLive", b.id));
                }
            }
            // Scalar-position consts mirror lower()'s loud pre-checks.
            // Calls without signatures mirror lower()'s CallNoSig rule.
            // Terminator dests join the bound set exactly like lower().
            match &b.terminator {
                Term::Call { args, sig, dest, .. } => {
                    if args.iter().any(const_needs_loud) {
                        tot_unsup += 1;
                        unsup.push(format!("{}: T/CallConstValue", b.id));
                    } else if call_sig_loud(sig, args) {
                        tot_unsup += 1;
                        unsup.push(format!("{}: T/CallNoSig", b.id));
                    }
                    if let Some(d) = dest {
                        if bound.contains(d) {
                            tot_unsup += 1;
                            unsup.push(format!("{}:{} Rebind", b.id, d));
                        } else {
                            bound.insert(d.clone());
                        }
                    }
                }
                Term::Assert { cond, .. } if const_needs_loud(cond) => {
                    tot_unsup += 1;
                    unsup.push(format!("{}: T/AssertConstValue", b.id));
                }
                Term::SwitchInt { discr, .. } if const_needs_loud(discr) => {
                    tot_unsup += 1;
                    unsup.push(format!("{}: T/SwitchConstValue", b.id));
                }
                _ => {}
            }
            if let Term::InlineAsm { text, template, options, modifiers, inout, outs, ins, .. } = &b.terminator {
                // Mirror lower(): exact-mov and value-stable inout pass, the rest is counted.
                // Dests join the bound set exactly like lower().
                let t = template.as_deref();
                let o = options.as_deref();
                let bound_dest: Option<String> =
                    asm_mov_copy(t, o, *modifiers, *inout, outs, ins).map(|(d, _)| d).or_else(|| {
                        asm_inout_passthrough(t, o, *modifiers, *inout, outs, ins)
                            .and_then(|(d, _)| d)
                    });
                match bound_dest {
                    Some(d) => {
                        if bound.contains(&d) {
                            tot_unsup += 1;
                            unsup.push(format!("{}:{} Rebind", b.id, d));
                        } else {
                            bound.insert(d);
                        }
                    }
                    None => {
                        // Passthrough-comment or loud: check the loud case.
                        // Loud single-out placeholders join bound like lower().
                        let ok = asm_mov_copy(t, o, *modifiers, *inout, outs, ins).is_some()
                            || asm_inout_passthrough(t, o, *modifiers, *inout, outs, ins).is_some();
                        if !ok {
                            tot_unsup += 1;
                            unsup.push(format!("{}: T/InlineAsm({})", b.id, text.chars().take(60).collect::<String>()));
                            if outs.len() == 1 && is_plain_local(outs[0].trim()) {
                                bound.insert(outs[0].trim().to_string());
                            }
                        }
                    }
                }
            }
        }
        println!("fn {}: blocks={} stmts+terms={} unsupported={}", f.name, f.blocks.len(),
                 f.blocks.iter().map(|b| b.statements.len() + 1).sum::<usize>(), unsup.len());
        for (k, n) in &kinds {
            println!("    {:>4} {}", n, k);
        }
        for u in &unsup {
            println!("    ?? {}", u);
        }
    }
    println!("TOTAL stmts={} terms={} unsupported={} coverage={:.1}%", tot_stmts, tot_terms, tot_unsup,
             100.0 * (tot_stmts + tot_terms - tot_unsup) as f64 / (tot_stmts + tot_terms).max(1) as f64);
    ExitCode::SUCCESS
}

fn cmd_parse(args: &[String]) -> ExitCode {
    let (mut input, mut fname, mut out) = (None, "main".to_string(), None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--fn" => { i += 1; if let Some(v) = args.get(i) { fname = v.clone(); } }
            "--out" => { i += 1; out = args.get(i).cloned(); }
            s if !s.starts_with("--") && input.is_none() => input = Some(s.to_string()),
            _ => {}
        }
        i += 1;
    }
    let (input, out) = match (input, out) {
        (Some(a), Some(b)) => (a, b),
        _ => { eprintln!("usage: mir2sa parse <mir-text> --fn <name> --out <mir.json>"); return ExitCode::from(2); }
    };
    let text = match std::fs::read_to_string(&input) {
        Ok(t) => t,
        Err(e) => { eprintln!("read {}: {}", input, e); return ExitCode::from(2); }
    };
    let body = match extract_fn(&text, &fname) {
        Some(b) => b,
        None => { eprintln!("function {} not found", fname); return ExitCode::from(2); }
    };
    let (mut blocks, mut cur, mut bid, mut total_uns) = (vec![], None::<Vec<String>>, String::new(), 0);
    for ln in &body {
        if let Some(id) = is_bb_header(ln) {
            if let Some(prev) = cur.take() {
                let (stmts, term, u) = parse_block(&prev);
                blocks.push(Block { id: std::mem::replace(&mut bid, id), statements: stmts, terminator: term });
                total_uns += u;
            } else {
                bid = id;
            }
            cur = Some(vec![]);
        } else if cur.is_some() {
            let t = ln.trim();
            if t != "}" && t != "{" {
                cur.as_mut().unwrap().push(ln.clone());
            }
        }
    }
    if let Some(prev) = cur.take() {
        let (stmts, term, u) = parse_block(&prev);
        blocks.push(Block { id: bid, statements: stmts, terminator: term });
        total_uns += u;
    }
    let nstmt: usize = blocks.iter().map(|b| b.statements.len()).sum();
    let mir = MirFile { source: Some("rustc -Zunpretty=mir (real compiler output)".to_string()), functions: vec![Function { name: fname, locals: vec![], params: vec![], ret: None, sig_ok: true, blocks }] };
    let json = serde_json::to_string_pretty(&mir).unwrap();
    if std::fs::write(&out, json).is_err() {
        eprintln!("write {}", out);
        return ExitCode::from(2);
    }
    println!("blocks={} stmts={} unsupported={} -> {}", mir.functions[0].blocks.len(), nstmt, total_uns, out);
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: mir2sa <parse|lower|coverage> ...");
        return ExitCode::from(2);
    }
    match args[0].as_str() {
        "parse" => cmd_parse(&args[1..]),
        "lower" => cmd_lower(&args[1..]),
        "coverage" => cmd_coverage(&args[1..]),
        _ => { eprintln!("unknown subcommand {}", args[0]); ExitCode::from(2) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::render_rvalue;

    fn c(v: &str) -> Operand {
        Operand::Const { value: v.to_string(), str_bytes: None, str_len: None }
    }

    fn sc(v: &str, bytes: &[u64]) -> Operand {
        Operand::Const { value: v.to_string(), str_bytes: Some(bytes.to_vec()), str_len: Some(bytes.len() as u64) }
    }

    #[test]
    fn scalar_hex_driver_form() {
        assert_eq!(scalar_hex_value("Val(Scalar(0x00000001), i32)", "i32"), Some(1));
        assert_eq!(scalar_hex_value("Val(Scalar(0x01), bool)", "bool"), Some(1));
        assert_eq!(scalar_hex_value("Val(Scalar(0x00000001), u64)", "i32"), None);
    }

    #[test]
    fn const_elem_both_forms() {
        assert_eq!(const_array_elem("1_i32", "i32", 4).as_deref(), Some("1"));
        assert_eq!(const_array_elem("Val(Scalar(0x00000002), i32)", "i32", 4).as_deref(), Some("2"));
        assert_eq!(const_array_elem("true", "bool", 1).as_deref(), Some("1"));
    }

    #[test]
    fn repeat_forms() {
        assert_eq!(repeat_plan("7_u8", "8"), Some(("7".to_string(), 8)));
        assert_eq!(repeat_plan("Val(Scalar(0x07), u8)", "8_usize"), Some(("7".to_string(), 8)));
        assert_eq!(repeat_plan("Val(Scalar(0x07), u8)", "Val(Scalar(0x08), usize)"), Some(("7".to_string(), 8)));
        assert!(repeat_plan("x", "8").is_none());
        assert!(repeat_plan("7_u8", "N").is_none());
    }

    #[test]
    fn array_init_bb30_shape() {
        let place = "(((*_46).1: X<[i32; 3]>).0: [i32; 3])";
        let elems = vec![
            c("Val(Scalar(0x00000001), i32)"),
            c("Val(Scalar(0x00000002), i32)"),
            c("Val(Scalar(0x00000003), i32)"),
        ];
        let lines = lower_array_init("bb30", place, &elems).expect("must lower");
        assert_eq!(lines[1], "_agg_bb30 = alloc 12");
        assert_eq!(lines[2], "store _agg_bb30+0, 1 as i32");
        assert_eq!(lines[4], "store _agg_bb30+8, 3 as i32");
    }

    #[test]
    fn adt_range_two_i32() {
        // f_loops bb10: Range {0, 4} -> alloc 8, packed i32+i32, dest bound.
        let elems = vec![
            c("Val(Scalar(0x00000000), i32)"),
            c("Val(Scalar(0x00000004), i32)"),
        ];
        let lines = lower_adt_init("bb10", "_19", "_19", &elems, None).expect("range must lower");
        assert_eq!(lines[1], "_agg_bb10 = alloc 8");
        assert_eq!(lines[2], "store _agg_bb10+0, 0 as i32");
        assert_eq!(lines[3], "store _agg_bb10+4, 4 as i32");
        assert_eq!(lines[4], "_19 = _agg_bb10");
    }

    #[test]
    fn adt_mixed_move_const_bool() {
        // main bb13: (1, Move _38, true) -> i32 / u64-move / u8(bool).
        // sla ABI: i32@0 (4B), u64@8 (align 8), u8@16 (packed).
        let elems = vec![
            c("Val(Scalar(0x00000001), i32)"),
            Operand::Move { place: "_38".to_string() },
            c("Val(Scalar(0x01), bool)"),
        ];
        let lines = lower_adt_init("bb13", "_37", "_37", &elems, None).expect("mixed tuple must lower");
        assert_eq!(lines[1], "_agg_bb13 = alloc 17");
        assert_eq!(lines[2], "store _agg_bb13+0, 1 as i32");
        assert_eq!(lines[3], "store _agg_bb13+8, ^_38 as u64");
        assert_eq!(lines[4], "store _agg_bb13+16, 1 as u8");
        assert_eq!(lines[5], "_37 = _agg_bb13");
    }

    #[test]
    fn adt_generic_two_moves() {
        // f_generic bb2: (Move _2, Move _4) -> two pointer slots, moves visible.
        let elems = vec![
            Operand::Move { place: "_2".to_string() },
            Operand::Move { place: "_4".to_string() },
        ];
        let lines = lower_adt_init("bb2", "_0", "_0", &elems, None).expect("generic tuple must lower");
        assert_eq!(lines[1], "_agg_bb2 = alloc 16");
        assert_eq!(lines[2], "store _agg_bb2+0, ^_2 as u64");
        assert_eq!(lines[3], "store _agg_bb2+8, ^_4 as u64");
    }

    #[test]
    fn thread_local_registry_call() {
        // ThreadLocal rvalue -> registry call with stable FNV-1a key, no UNSUPPORTED.
        let def = "TLS_N::{constant#0}::{closure#0}::__RUST_STD_INTERNAL_VAL";
        assert_eq!(tls_key(def), tls_key(def));
        assert_ne!(tls_key(def), tls_key("TLS_N::{constant#0}::{closure#1}::__RUST_STD_INTERNAL_VAL"));
        let mut unsup = vec![];
        let rv = Rvalue::ThreadLocal { def: def.to_string() };
        let line = render_rvalue(&rv, "_3", Some("_3"), &mut unsup, "bb0", &mut 0usize, &std::collections::HashMap::new(), &std::collections::BTreeMap::new());
        assert!(unsup.is_empty());
        assert_eq!(line, format!("// thread-local registry slot\n_3 = call @sa_thread_local_slot({})", tls_key(def)));
    }

    fn asm_mov(
        template: Option<&str>,
        options: Option<&str>,
        modifiers: bool,
        outs: &[&str],
        ins: &[Operand],
    ) -> Option<(String, String)> {
        let outs: Vec<String> = outs.iter().map(|s| s.to_string()).collect();
        asm_mov_copy(template, options, modifiers, false, &outs, ins)
    }

    fn asm_inout(
        template: Option<&str>,
        options: Option<&str>,
        modifiers: bool,
        inout: bool,
        outs: &[&str],
        ins: &[Operand],
    ) -> Option<(Option<String>, Option<String>)> {
        let outs: Vec<String> = outs.iter().map(|s| s.to_string()).collect();
        asm_inout_passthrough(template, options, modifiers, inout, &outs, ins)
    }

    #[test]
    fn asm_mov_copy_exact() {
        // f_asm bb0 shape: mov {0}, {1}, out _2, in copy _1, no options.
        let ins = vec![Operand::Copy { place: "_1".to_string() }];
        let (d, s) = asm_mov(Some("mov {0}, {1}"), Some(""), false, &["_2"], &ins)
            .expect("corpus mov must lower");
        assert_eq!(d, "_2");
        assert_eq!(s, "_1");
    }

    #[test]
    fn asm_mov_plain_copy() {
        // SA `=` already moves (`_x = ^_y` is UnknownRegister); Move renders
        // as a plain place in rvalue position, `^` lives only in call args.
        let ins = vec![Operand::Move { place: "_1".to_string() }];
        let (_, s) = asm_mov(Some("mov {0}, {1}"), Some(""), false, &["_2"], &ins)
            .expect("move input must lower");
        assert_eq!(s, "_1");
    }

    #[test]
    fn asm_non_mov_stays_loud() {
        let ins = vec![Operand::Copy { place: "_1".to_string() }];
        // Old-schema fixture (no structured fields) -> None -> UNSUPPORTED, counted.
        assert!(asm_mov(None, None, false, &[], &[]).is_none());
        assert!(asm_mov(Some("add {0}, {1}"), Some(""), false, &["_2"], &ins).is_none());
        assert!(asm_mov(Some("mov {0}, {1}"), Some("PURE"), false, &["_2"], &ins).is_none());
        assert!(asm_mov(Some("mov {0}, {1}"), Some(""), true, &["_2"], &ins).is_none());
        assert!(asm_mov(Some("mov {0}, {1}"), Some(""), false, &["_proj"], &ins).is_none());
        assert!(asm_mov(Some("mov {0}, {1}"), Some(""), false, &["_2", "_3"], &ins).is_none());
    }

    #[test]
    fn asm_inout_passthrough_117() {
        // sla-117 shape: comment-only template, inout same local -> comment, no instr.
        let ins = vec![Operand::Copy { place: "_1".to_string() }];
        assert_eq!(
            asm_inout(Some("/* native escape */"), Some(""), false, true, &["_1"], &ins),
            Some((None, None))
        );
    }

    #[test]
    fn asm_inout_split_places() {
        // Same gate, split places -> materialized copy (plain `=` moves).
        let ins = vec![Operand::Move { place: "_1".to_string() }];
        assert_eq!(
            asm_inout(Some("/* nop */"), Some(""), false, true, &["_2"], &ins),
            Some((Some("_2".to_string()), Some("_1".to_string())))
        );
    }

    #[test]
    fn asm_inout_non_passthrough_stays_loud() {
        let ins = vec![Operand::Copy { place: "_1".to_string() }];
        // Not inout -> the inout gate refuses (mov gate may still apply elsewhere).
        assert_eq!(asm_inout(Some("mov {0}, {1}"), Some(""), false, false, &["_2"], &ins), None);
        // Real instruction in template -> loud.
        assert_eq!(asm_inout(Some("xchg {0}, {1}"), Some(""), false, true, &["_1"], &ins), None);
        // Unclosed comment -> loud.
        assert_eq!(asm_inout(Some("/* oops"), Some(""), false, true, &["_1"], &ins), None);
        // Options set -> loud.
        assert_eq!(asm_inout(Some("/* nop */"), Some("NOMEM"), false, true, &["_1"], &ins), None);
        // Modifiers set -> loud.
        assert_eq!(asm_inout(Some("/* nop */"), Some(""), true, true, &["_1"], &ins), None);
        // Bad dest -> loud.
        assert_eq!(asm_inout(Some("/* nop */"), Some(""), false, true, &["_proj"], &ins), None);
    }

    #[test]
    fn asm_versioned_names_pass_gates() {
        // Versioned renames (`_1_v1`) are plain locals for gate purposes;
        // otherwise versioning would silence every gated pattern.
        let ins = vec![Operand::Copy { place: "_1_v0".to_string() }];
        assert_eq!(
            asm_inout(Some("/* nop */"), Some(""), false, true, &["_1_v1"], &ins),
            Some((Some("_1_v1".to_string()), Some("_1_v0".to_string())))
        );
        let outs: Vec<String> = vec!["_2_v0".to_string()];
        let ins2 = vec![Operand::Copy { place: "_1_v0".to_string() }];
        assert!(crate::asm::asm_mov_copy(
            Some("mov {0}, {1}"), Some(""), false, false, &outs, &ins2
        ).is_some());
    }

    #[test]
    fn adt_zst_skipped() {
        // (8_i32, PhantomPinned): ZST occupies 0 bytes, emits no store.
        let elems = vec![
            c("Val(Scalar(0x00000008), i32)"),
            c("Val(ZeroSized, std::marker::PhantomPinned)"),
        ];
        let lines = lower_adt_init("bb0", "_2", "_2", &elems, None).expect("zst pair must lower");
        assert_eq!(lines[1], "_agg_bb0 = alloc 4");
        assert_eq!(lines[2], "store _agg_bb0+0, 8 as i32");
        assert_eq!(lines[3], "_2 = _agg_bb0");
    }

    fn adt_layout(size: u64, offsets: &[u64]) -> AdtLayout {
        AdtLayout { size, offsets: offsets.to_vec() }
    }

    #[test]
    fn adt_v2_reordered_tuple() {
        // main bb13 shape: (1_i32, Move String, true) reordered by rustc to
        // [24, 0, 28] size 32. v1 would emit alloc 17 — v2 must win verbatim.
        let elems = vec![
            c("Val(Scalar(0x00000001), i32)"),
            Operand::Move { place: "_38".to_string() },
            c("Val(Scalar(0x01), bool)"),
        ];
        let layout = adt_layout(32, &[24, 0, 28]);
        let lines = lower_adt_init("bb13", "_37", "_37", &elems, Some(&layout))
            .expect("v2 layout must lower");
        assert_eq!(lines[1], "_agg_bb13 = alloc 32");
        assert_eq!(lines[2], "store _agg_bb13+24, 1 as i32");
        assert_eq!(lines[3], "store _agg_bb13+0, ^_38 as u64");
        assert_eq!(lines[4], "store _agg_bb13+28, 1 as u8");
        assert!(lines[0].contains("p_layout v2"));
    }

    #[test]
    fn adt_v2_enum_payload_absolute() {
        // Shape::Point(1, 2): payload offsets absolute in the 12B enum
        // (tag gap at 0..4), size is the full enum size.
        let elems = vec![
            c("Val(Scalar(0x00000001), i32)"),
            c("Val(Scalar(0x00000002), i32)"),
        ];
        let layout = adt_layout(12, &[4, 8]);
        let lines = lower_adt_init("bb0", "_6", "_6", &elems, Some(&layout))
            .expect("enum payload must lower");
        assert_eq!(lines[1], "_agg_bb0 = alloc 12");
        assert_eq!(lines[2], "store _agg_bb0+4, 1 as i32");
        assert_eq!(lines[3], "store _agg_bb0+8, 2 as i32");
    }

    #[test]
    fn adt_v2_arity_mismatch_falls_back() {
        // Layout arity != operand count -> v1 heuristic (must not miscompile).
        let elems = vec![
            c("Val(Scalar(0x00000000), i32)"),
            c("Val(Scalar(0x00000004), i32)"),
        ];
        let layout = adt_layout(8, &[0]);
        let lines = lower_adt_init("bb10", "_19", "_19", &elems, Some(&layout))
            .expect("mismatch must fall back to v1");
        assert!(lines[0].contains("p_layout v1"));
        assert_eq!(lines[1], "_agg_bb10 = alloc 8");
    }

    #[test]
    fn adt_str_lit_fat_ptr() {
        // 55_builder `new`: ("GET", "/") with v2 layout — byte buffers plus
        // (ptr,len) double stores per slice.sal.
        let elems = vec![
            sc("Val(Slice { alloc_id: alloc1, meta: 3 }, &'erased str)", &[71, 69, 84]),
            sc("Val(Slice { alloc_id: alloc2, meta: 1 }, &'erased str)", &[47]),
        ];
        let layout = adt_layout(32, &[0, 16]);
        let lines = lower_adt_init("bb0", "_0", "_0", &elems, Some(&layout))
            .expect("str pair must lower");
        assert_eq!(lines[1], "_agg_bb0 = alloc 32");
        assert_eq!(lines[2], "_str_bb0_0 = alloc 3");
        assert_eq!(lines[3], "store _str_bb0_0+0, 71 as u8");
        assert_eq!(lines[5], "store _str_bb0_0+2, 84 as u8");
        assert_eq!(lines[6], "store _agg_bb0+0, _str_bb0_0 as ptr");
        assert_eq!(lines[7], "store _agg_bb0+8, 3 as u64");
        assert_eq!(lines[8], "_str_bb0_1 = alloc 1");
        assert_eq!(lines[10], "store _agg_bb0+16, _str_bb0_1 as ptr");
        assert_eq!(lines[11], "store _agg_bb0+24, 1 as u64");
        assert_eq!(lines[12], "_0 = _agg_bb0");
    }

    #[test]
    fn adt_str_lit_gates() {
        // Over-long literals and count mismatches stay loud (None).
        let big = vec![97u64; 65];
        let elems = vec![
            sc("Val(Slice { alloc_id: alloc9, meta: 65 }, &'erased str)", &big),
            c("Val(Scalar(0x00000001), i32)"),
        ];
        assert!(lower_adt_init("bb0", "_1", "_1", &elems, None).is_none());
        // Driver/bytes count mismatch (2 bytes, len 3) stays loud.
        let elems = vec![
            Operand::Const {
                value: "Val(Slice { alloc_id: alloc1, meta: 3 }, &'erased str)".to_string(),
                str_bytes: Some(vec![71, 69]),
                str_len: Some(3),
            },
            c("Val(Scalar(0x00000001), i32)"),
        ];
        assert!(lower_adt_init("bb0", "_1", "_1", &elems, None).is_none());
    }

    fn blank_fn(name: &str, blocks: Vec<Block>) -> Function {
        Function { name: name.to_string(), locals: vec![], params: vec![], ret: None, sig_ok: true, blocks }
    }

    fn blank_block(id: &str, term: Term) -> Block {
        Block { id: id.to_string(), statements: vec![], terminator: term }
    }

    #[test]
    fn sa_ident_sanitizes() {
        assert_eq!(sa_ident("{closure#0}"), "_closure_0_");
        assert_eq!(sa_ident("{constant#1}"), "_constant_1_");
        assert_eq!(sa_ident("f_match"), "f_match");
        // Labels always carry the L_ prefix (bare `bb0:` is ForbiddenSyntax).
        assert_eq!(sa_label("bb0"), "L_bb0");
        assert_eq!(sa_label("sw_bb0_1"), "L_sw_bb0_1");
    }

    #[test]
    fn switchint_chain_shape() {
        // eq + two-target br per arm, fallthrough labels, no inline `==`.
        let f = blank_fn("f_sw", vec![
            blank_block("bb0", Term::SwitchInt {
                discr: Box::new(Operand::Copy { place: "_1".to_string() }),
                targets: vec![("0".to_string(), "bb1".to_string()), ("1".to_string(), "bb2".to_string())],
                otherwise: "bb3".to_string(),
            }),
            blank_block("bb1", Term::Return { ret: None }),
            blank_block("bb2", Term::Return { ret: None }),
            blank_block("bb3", Term::Return { ret: None }),
        ]);
        let mut unsup = vec![];
        let sa = lower_function(&f, &mut unsup);
        assert!(unsup.is_empty());
        assert!(sa.contains("L_bb0:"), "label shape:\n{}", sa);
        assert!(!sa.contains("=="), "no inline ==:\n{}", sa);
        assert!(sa.contains("_sw_bb0_eq_0 = eq _1, 0"), "eq arm:\n{}", sa);
        assert!(sa.contains("br _sw_bb0_eq_0 -> L_bb1, L_sw_bb0_0"), "two-target br:\n{}", sa);
        assert!(sa.contains("L_sw_bb0_0:"), "fallthrough label:\n{}", sa);
    }

    fn ret_fn(name: &str, ret: Option<&str>, blocks: Vec<Block>) -> Function {
        let mut f = blank_fn(name, blocks);
        f.ret = ret.map(|s| s.to_string());
        f
    }

    fn def(dest: &str, v: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: Some(dest.to_string()),
            rvalue: Rvalue::Use { op: c(v) },
        }
    }

    /// Plain register copy (not a const literal: `c()` values go loud).
    fn def_from(dest: &str, src: &str) -> Stmt {
        Stmt::Assign {
            dest: dest.to_string(),
            dest_place: Some(dest.to_string()),
            rvalue: Rvalue::Use { op: Operand::Copy { place: src.to_string() } },
        }
    }

    #[test]
    fn return_value_emitted() {
        // Probe r4: `return <reg>` is legal and consumes the reg (r5 shows
        // `return 0` leaks it), so the resolved return local is emitted.
        let f = ret_fn("f_ret", Some("i32"), vec![
            blank_block("bb0", Term::Goto { target: "bb1".to_string() }),
            Block {
                id: "bb1".to_string(),
                statements: vec![def_from("_0", "_1")],
                terminator: Term::Return { ret: Some("_0".to_string()) },
            },
        ]);
        let mut f = f;
        f.params = vec!["i32".to_string()];
        let mut unsup = vec![];
        let sa = lower_function(&f, &mut unsup);
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert!(sa.contains("return _0"), "return value:\n{}", sa);
    }

    #[test]
    fn return_fallbacks_are_safe() {
        // Void: no register to return. Unbound reg: fall back. Conflict:
        // loud marker plus the `0` fallback (never a raw sentinel register).
        let cases: Vec<(Function, &str)> = vec![
            (ret_fn("f_void", Some("void"), vec![Block {
                id: "bb0".to_string(),
                statements: vec![def_from("_0", "_1")],
                terminator: Term::Return { ret: Some("_0".to_string()) },
            }]), "return 0"),
            (ret_fn("f_unbound", Some("i32"), vec![
                blank_block("bb0", Term::Return { ret: Some("_0".to_string()) }),
            ]), "return 0"),
            (ret_fn("f_conf", Some("i32"), vec![Block {
                id: "bb0".to_string(),
                statements: vec![def_from("_0", "_1")],
                terminator: Term::Return { ret: Some("__VERSION_CONFLICT__".to_string()) },
            }]), "return 0"),
        ];
        for (f, want) in &cases {
            let mut unsup = vec![];
            let sa = lower_function(f, &mut unsup);
            assert!(sa.contains(want), "fallback {} missing:\n{}", want, sa);
            assert!(!sa.contains("__VERSION_CONFLICT__"), "no raw sentinel:\n{}", sa);
        }
        let mut unsup = vec![];
        let _ = lower_function(&cases[2].0, &mut unsup);
        assert_eq!(unsup, vec!["bb0: ReturnConflict".to_string()]);
    }

    #[test]
    fn ptr_metadata_reads_offset_8() {
        // Fat-pointer meta is the (ptr,len) tail: exact `load p+8` (no SA
        // mnemonic exists). Probe m1/m2 confirm the shape assembles.
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = render_rvalue(
            &Rvalue::UnOp {
                op: "PtrMetadata".to_string(),
                operand: Box::new(Operand::Copy { place: "_3".to_string() }),
            },
            "_2", Some("_2"), &mut unsup, "bb0", &mut idx,
            &std::collections::HashMap::new(), &std::collections::BTreeMap::new(),
        );
        assert!(unsup.is_empty(), "{:?}", unsup);
        assert_eq!(line, "_2 = load _3+8 as u64");
        // A Move operand binds a temp first (plain `=` moves).
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = render_rvalue(
            &Rvalue::UnOp {
                op: "PtrMetadata".to_string(),
                operand: Box::new(Operand::Move { place: "_3".to_string() }),
            },
            "_2", Some("_2"), &mut unsup, "bb0", &mut idx,
            &std::collections::HashMap::new(), &std::collections::BTreeMap::new(),
        );
        assert_eq!(line, "_mv_bb0_0 = _3\n_2 = load _mv_bb0_0+8 as u64");
    }

    #[test]
    fn loud_call_block_defs_still_exit_freed() {
        // T23: a block whose terminator goes loud takes the `continue` path in
        // lower(), so its emitted lines used to be missing from drop.rs's input
        // and every register defined there was never exit-freed (a leak as soon
        // as it holds a real allocation). Block ranges now close lazily.
        let loud = Operand::Const {
            value: "Unevaluated(UnevaluatedConst { def: DefId(0:1 })".to_string(),
            str_bytes: None,
            str_len: None,
        };
        let f = blank_fn("f_loudblk", vec![
            Block {
                id: "bb0".to_string(),
                statements: vec![def_from("_2", "_1")],
                terminator: Term::Call {
                    func: "g".to_string(),
                    func_raw: None,
                    args: vec![loud],
                    dest: Some("_3".to_string()),
                    target: Some("bb1".to_string()),
                    sig: None,
                },
            },
            blank_block("bb1", Term::Return { ret: None }),
        ]);
        let mut f = f;
        f.params = vec!["ptr".to_string()];
        let mut unsup = vec![];
        let sa = lower_function(&f, &mut unsup);
        assert!(unsup.iter().any(|u| u.contains("CallConstValue")), "{:?}", unsup);
        assert!(sa.contains("!_2"), "reg defined in a loud block must be exit-freed:\n{}", sa);
    }

    #[test]
    fn cast_reload_binds_temp_for_convert() {
        // sla-201 shape: a spilled source feeding a WIDTH-CHANGING cast must
        // be bound to a temp first — `zext load _s+0 as u8 as i32` does not
        // parse (UnknownRegister). A plain-copy cast may inline the load.
        let spilled = std::collections::BTreeMap::from([("_2".to_string(), "u8".to_string())]);
        let cast = |kind: &str, src: &str, ty: &str| Rvalue::Cast {
            op: Box::new(Operand::Copy { place: "_2".to_string() }),
            ty: ty.to_string(),
            castkind: Some(kind.to_string()),
            src_ty: Some(src.to_string()),
        };
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = render_rvalue(
            &cast("IntToInt", "u8", "i32"), "_7", Some("_7"), &mut unsup, "bb0",
            &mut idx, &std::collections::HashMap::new(), &spilled,
        );
        assert!(unsup.is_empty());
        assert_eq!(line, "_mv_bb0_0 = load _2_spill+0 as u8\n_7 = zext _mv_bb0_0 as i32");
        let mut idx = 0usize;
        let mut unsup = vec![];
        let line = render_rvalue(
            &cast("IntToInt", "u32", "u32"), "_7", Some("_7"), &mut unsup, "bb0",
            &mut idx, &std::collections::HashMap::new(), &spilled,
        );
        assert_eq!(line, "_7 = load _2_spill+0 as u8");
    }

    #[test]
    fn assert_shape() {
        // `assert cond` is not an instruction: eq + br + numeric panic.
        let f = blank_fn("f_as", vec![
            blank_block("bb0", Term::Assert {
                cond: Box::new(Operand::Copy { place: "_1".to_string() }),
                target: "bb1".to_string(),
                msg: Some("Overflow(Add, copy _1, const 1_i32)".to_string()),
                expected: Some(true),
            }),
            blank_block("bb1", Term::Return { ret: None }),
        ]);
        let mut unsup = vec![];
        let sa = lower_function(&f, &mut unsup);
        assert!(unsup.is_empty());
        assert!(!sa.contains("assert _1"), "no bare assert:\n{}", sa);
        assert!(sa.contains("_as_bb0_eq = eq _1, 1"), "eq:\n{}", sa);
        assert!(sa.contains("br _as_bb0_eq -> L_bb1, L_as_bb0_fail"), "br:\n{}", sa);
        assert!(sa.contains("panic(1500)"), "numeric panic:\n{}", sa);
    }

    #[test]
    fn unreachable_becomes_panic() {
        // Bare `unreachable` ends the SA function textually; panic diverges
        // without ending it, so siblings keep assembling.
        let f = blank_fn("f_un", vec![
            blank_block("bb0", Term::Unreachable),
            blank_block("bb1", Term::Return { ret: None }),
        ]);
        let mut unsup = vec![];
        let sa = lower_function(&f, &mut unsup);
        assert!(unsup.is_empty());
        assert!(!sa.contains("\n    unreachable\n"), "no bare unreachable:\n{}", sa);
        assert!(sa.contains("panic(1600)"), "panic:\n{}", sa);
    }

    #[test]
    fn typed_header_and_extern() {
        // MIR params bind in a typed header; callees get typed @extern decls.
        let f = blank_fn("f_typed", vec![
            blank_block("bb0", Term::Call {
                func: "some_fn".to_string(),
                func_raw: None,
                args: vec![Operand::Copy { place: "_1".to_string() }],
                dest: Some("_0".to_string()),
                target: Some("bb1".to_string()),
                sig: Some(CallSig { params: vec!["ptr".to_string()], ret: "i32".to_string() }),
            }),
            blank_block("bb1", Term::Return { ret: None }),
        ]);
        let mut f = f;
        f.params = vec!["ptr".to_string()];
        f.ret = Some("i32".to_string());
        let mut unsup = vec![];
        let sa = lower_function(&f, &mut unsup);
        assert!(unsup.is_empty());
        assert!(sa.contains("@f_typed(_1: ptr) -> i32:"), "header:\n{}", sa);
        let mir = MirFile { source: None, functions: vec![f] };
        let exts = collect_externs(&mir);
        assert_eq!(
            exts.get("some_fn"),
            Some(&Some((vec!["ptr".to_string()], "i32".to_string())))
        );
        assert_eq!(extern_decl("some_fn", exts.get("some_fn").unwrap()), "@extern some_fn(_a0: ptr) -> i32");
    }

    #[test]
    fn cast_copy_vs_convert() {
        use crate::asm::CastLower;
        // Same-width and pointer identities are plain copies.
        assert!(matches!(lower_cast("PtrToPtr", "*", "*const ()"), Some(CastLower::Copy)));
        assert!(matches!(lower_cast("IntToInt", "i32", "u32"), Some(CastLower::Copy)));
        // Width changes pick by signedness.
        assert!(matches!(lower_cast("IntToInt", "i32", "i64"), Some(CastLower::Convert("sext"))));
        assert!(matches!(lower_cast("IntToInt", "u32", "u64"), Some(CastLower::Convert("zext"))));
        assert!(matches!(lower_cast("IntToInt", "i64", "i32"), Some(CastLower::Convert("trunc"))));
        // Unsized/fn-ptr coercions stay loud.
        assert!(lower_cast("PointerCoercion(Unsize, Implicit)", "&", "&'_ str").is_none());
    }
}
