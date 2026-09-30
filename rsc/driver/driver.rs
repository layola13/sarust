//! rsc_driver: real rustc_private driver — `.rs -> MIR -> mir.json`.
//!
//! Official compiler does 100% checking (parse/resolve/hir/typeck/borrowck);
//! we only hijack the resulting MIR in `after_analysis` and dump the stable
//! mir.json subset consumed by `mir2sa lower`. No codegen runs
//! (`Compilation::Stop`). If borrowck/typeck reported errors, no mir.json is
//! written (zero-error backend input, guaranteed).
//!
//! Build (needs nightly + rustc-dev, both present):
//!   ./build.sh
//! Run:
//!   ./target/rsc_driver --edition 2021 /tmp/rsc_demo/src/main.rs \
//!       --rsc-out /tmp/driver.mir.json --crate-type bin

#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_ast;
extern crate rustc_abi;

use rustc_ast::ast::InlineAsmTemplatePiece;

use rustc_driver::{Callbacks, Compilation, run_compiler};
use rustc_hir::def::DefKind;
use rustc_interface::interface::Compiler;
use rustc_middle::mir::{
    BasicBlock, Body, BorrowKind, Const, ConstOperand, InlineAsmOperand, Local,
    Operand, Place, Rvalue, StatementKind, TerminatorKind,
};
use rustc_middle::ty::{ConstKind, TyCtxt, TyKind};
use std::fmt::Write as _;

struct RscCallbacks {
    out: String,
}

fn esc(s: &str, into: &mut String) {
    for c in s.chars() {
        match c {
            '"' => into.push_str("\\\""),
            '\\' => into.push_str("\\\\"),
            '\n' => into.push_str("\\n"),
            '\r' => into.push_str("\\r"),
            '\t' => into.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(into, "\\u{:04x}", c as u32).unwrap(),
            c => into.push(c),
        }
    }
}

fn trunc(s: String, n: usize) -> String {
    if s.len() > n { s[..n].to_string() } else { s }
}

fn local_name(l: Local) -> String {
    format!("_{}", l.as_u32())
}

/// (short, full): short is the base local when projectionless.
/// Pure deref chains `(*_N)` resolve to `_N` (reborrow keeps the borrow
/// graph connected; full Deref lowering is p_layout). See place_via().
fn place_name(p: &Place<'_>) -> (String, String) {
    let full = trunc(format!("{:?}", p), 80);
    match p.as_local() {
        Some(l) => (local_name(l), full),
        None => {
            // Best effort: leading `_N` token (mir2sa's base_local agrees).
            let t = full.trim_start_matches('(');
            let mut digits = String::new();
            let mut it = t.chars();
            if it.next() == Some('_') {
                for c in it {
                    if c.is_ascii_digit() {
                        digits.push(c);
                    } else {
                        break;
                    }
                }
            }
            if !digits.is_empty() {
                return (format!("_{}", digits), full);
            }
            // Pure deref `(*_N)`: base local (provenance in place_via).
            let d = full.trim().trim_start_matches('(').trim_end_matches(')');
            if let Some(inner) = d.strip_prefix('*') {
                let inner = inner.trim();
                if inner.starts_with('_')
                    && inner[1..].chars().all(|c| c.is_ascii_digit())
                    && !inner[1..].is_empty()
                {
                    return (inner.to_string(), full);
                }
            }
            ("_proj".to_string(), full)
        }
    }
}

/// Original place text when place_name had to approximate (deref chains,
/// `_proj` fallbacks); None for plain locals. Never silent.
fn place_via(p: &Place<'_>) -> Option<String> {
    match p.as_local() {
        Some(_) => None,
        None => Some(trunc(format!("{:?}", p), 80)),
    }
}

/// String-literal bytes for a `&str` slice constant (`Const::Val` with
/// `ConstValue::Slice`), e.g. promoted `"..."/utf8` literals. Returns
/// (bytes, len) only for valid UTF-8; anything else (unevaluated consts,
/// non-str slices, statics) yields None and the backend stays loud.
fn str_const_bytes<'a>(tcx: TyCtxt<'a>, c: &Const<'a>) -> Option<(&'a [u8], u64)> {
    use rustc_abi::Size;
    use rustc_middle::mir::interpret::{AllocRange, GlobalAlloc};
    let (cval, ty) = match c {
        Const::Val(v, ty) => (*v, *ty),
        _ => return None,
    };
    match ty.kind() {
        TyKind::Ref(_, inner, ty::Mutability::Not) if inner.is_str() => {}
        _ => return None,
    }
    let (alloc_id, meta) = match cval {
        rustc_middle::mir::ConstValue::Slice { alloc_id, meta } => (alloc_id, meta),
        _ => return None,
    };
    let mem = match tcx.global_alloc(alloc_id) {
        GlobalAlloc::Memory(m) => m,
        _ => return None,
    };
    let bytes = mem
        .inner()
        .get_bytes_unchecked(AllocRange { start: Size::ZERO, size: Size::from_bytes(meta) });
    std::str::from_utf8(bytes).ok()?;
    Some((bytes, meta))
}

fn operand_json<'a>(op: &Operand<'a>, tcx: TyCtxt<'a>, into: &mut String) {
    match op {
        Operand::Copy(p) => {
            let (s, _) = place_name(p);
            write!(into, "{{\"kind\": \"Copy\", \"place\": \"{}\"}}", s).unwrap();
        }
        Operand::Move(p) => {
            let (s, _) = place_name(p);
            write!(into, "{{\"kind\": \"Move\", \"place\": \"{}\"}}", s).unwrap();
        }
        Operand::Constant(c) => {
            let v = trunc(format!("{:?}", c.const_), 60);
            into.push_str("{\"kind\": \"Const\", \"value\": \"");
            esc(&v, into);
            into.push('"');
            // String-literal payload rides along only when resolvable;
            // absent fields keep old fixtures byte-compatible.
            if let Some((bytes, len)) = str_const_bytes(tcx, &c.const_) {
                into.push_str(", \"str_bytes\": [");
                for (i, b) in bytes.iter().enumerate() {
                    if i > 0 {
                        into.push_str(", ");
                    }
                    write!(into, "{}", b).unwrap();
                }
                write!(into, "], \"str_len\": {}", len).unwrap();
            }
            into.push('}');
        }
        Operand::RuntimeChecks(_) => {
            // Session-flag query operand (e.g. overflow-checks enabled?).
            // No SA const can name it: loud marker const, counted as APPROX.
            into.push_str("{\"kind\": \"Const\", \"value\": \"0 /*RuntimeChecks-unsupported*/\"}");
        }
    }
}

fn rvalue_json<'tcx>(
    rv: &Rvalue<'tcx>,
    tcx: TyCtxt<'tcx>,
    layout: Option<&(u64, Vec<u64>)>,
    into: &mut String,
) {
    match rv {
        Rvalue::Use(op, _) => {
            into.push_str("{\"kind\": \"Use\", \"op\": ");
            operand_json(op, tcx, into);
            into.push('}');
        }
        Rvalue::Ref(_, kind, p) => {
            let (s, _) = place_name(p);
            let m = matches!(kind, BorrowKind::Mut { .. });
            write!(into, "{{\"kind\": \"Ref\", \"place\": \"{}\"", s).unwrap();
            if m {
                into.push_str(", \"mut\": true");
            }
            if let Some(via) = place_via(p) {
                into.push_str(", \"via\": \"");
                esc(&via, into);
                into.push('"');
            }
            into.push('}');
        }
        Rvalue::RawPtr(kind, p) => {
            let (s, _) = place_name(p);
            let m = matches!(kind, rustc_middle::mir::RawPtrKind::Mut);
            write!(into, "{{\"kind\": \"RawPtr\", \"place\": \"{}\"", s).unwrap();
            if m {
                into.push_str(", \"mut\": true");
            }
            into.push('}');
        }
        Rvalue::Repeat(op, len) => {
            into.push_str("{\"kind\": \"Repeat\", \"op\": ");
            operand_json(op, tcx, into);
            into.push_str(", \"len\": \"");
            esc(&trunc(format!("{:?}", len), 40), into);
            into.push_str("\"}");
        }
        Rvalue::ThreadLocalRef(did) => {
            into.push_str("{\"kind\": \"ThreadLocal\", \"def\": \"");
            esc(&trunc(tcx.def_path_str(*did), 100), into);
            into.push_str("\"}");
        }
        Rvalue::BinaryOp(op, box_ops) => {
            into.push_str("{\"kind\": \"BinOp\", \"op\": \"");
            into.push_str(&format!("{:?}", op));
            into.push_str("\", \"left\": ");
            operand_json(&box_ops.0, tcx, into);
            into.push_str(", \"right\": ");
            operand_json(&box_ops.1, tcx, into);
            into.push('}');
        }
        Rvalue::UnaryOp(op, o) => {
            into.push_str("{\"kind\": \"UnOp\", \"op\": \"");
            into.push_str(&format!("{:?}", op));
            into.push_str("\", \"operand\": ");
            operand_json(o, tcx, into);
            into.push('}');
        }
        Rvalue::Cast(_, op, ty) => {
            into.push_str("{\"kind\": \"Cast\", \"op\": ");
            operand_json(op, tcx, into);
            into.push_str(", \"ty\": \"");
            esc(&trunc(format!("{:?}", ty), 60), into);
            into.push_str("\"}");
        }
        Rvalue::Aggregate(_kind, ops) => {
            into.push_str("{\"kind\": \"Aggregate\", \"elems\": [");
            for (i, o) in ops.iter().enumerate() {
                if i > 0 {
                    into.push_str(", ");
                }
                operand_json(o, tcx, into);
            }
            into.push_str("]");
            if let Some((size, offsets)) = layout {
                write!(into, ", \"layout\": {{\"size\": {}, \"offsets\": [", size).unwrap();
                for (i, o) in offsets.iter().enumerate() {
                    if i > 0 {
                        into.push_str(", ");
                    }
                    write!(into, "{}", o).unwrap();
                }
                into.push_str("]}");
            }
            into.push('}');
        }
        Rvalue::Discriminant(p) => {
            let (s, _) = place_name(p);
            write!(into, "{{\"kind\": \"Discriminant\", \"place\": \"{}\"}}", s).unwrap();
        }
        other => {
            into.push_str("{\"kind\": \"Unsupported\", \"text\": \"");
            esc(&trunc(format!("{:?}", other), 120), into);
            into.push_str("\"}");
        }
    }
}

fn fn_operand_name(tcx: TyCtxt<'_>, op: &Operand<'_>) -> (String, String) {
    // Prefer `a_b_c` path names; fall back to Debug text.
    if let Operand::Constant(c) = op {
        let c: &ConstOperand<'_> = c;
        if let Const::Ty(_, ct) = c.const_ {
            if let ConstKind::Value(ty::Value { ty, .. }) = ct.kind() {
            if let TyKind::FnDef(def_id, _) = ty.kind() {
                let full = tcx.def_path_str(*def_id);
                let short = full.rsplit("::").next().unwrap_or(&full).to_string();
                let clean: String = {
                    let mut s = String::new();
                    let mut sep = true;
                    for ch in short.chars() {
                        if ch.is_ascii_alphanumeric() || ch == '_' {
                            s.push(ch);
                            sep = false;
                        } else if !sep {
                            s.push('_');
                            sep = true;
                        }
                    }
                    while s.ends_with('_') {
                        s.pop();
                    }
                    s
                };
                return (clean, trunc(full, 100));
                }
            }
        }
    }
    let dbg = trunc(format!("{:?}", op), 100);
    (sanitize(&dbg), dbg)
}

fn sanitize(raw: &str) -> String {
    let mut s = String::new();
    let mut sep = true;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            s.push(ch);
            sep = false;
        } else if !sep {
            s.push('_');
            sep = true;
        }
    }
    while s.ends_with('_') {
        s.pop();
    }
    s.chars().take(80).collect()
}

fn bb_name(b: BasicBlock) -> String {
    format!("bb{}", b.index())
}

/// p_layout v2: true ADT field offsets for a multi-operand aggregate.
/// Resolves the destination value type against this body's decls and asks
/// rustc for the real layout (`tcx.layout_of`); enum payloads resolve
/// through the aggregate's variant index (the tag layout's fields would not
/// match the operands). Any failure (generics, unsized, const-only bodies)
/// yields None and mir2sa falls back to the v1 sla-ABI heuristic — the
/// driver never fails extraction because of layout.
fn aggregate_layout<'tcx>(
    tcx: TyCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    body: &Body<'tcx>,
    kind: &rustc_middle::mir::AggregateKind<'tcx>,
    dest: &Place<'tcx>,
    n: usize,
) -> Option<(u64, Vec<u64>)> {
    use rustc_middle::mir::AggregateKind;
    use rustc_middle::ty::PseudoCanonicalInput;
    use rustc_middle::ty::layout::LayoutCx;
    use rustc_middle::ty::TypingEnv;
    // MIR bodies here are destructor-observable monomorphic shapes; generic
    // params have no layout, so resolve under the fully-monomorphized env and
    // fall back (None) on any failure — extraction must never die on layout.
    let _ = param_env;
    let typing_env = TypingEnv::fully_monomorphized();
    let ty = dest.ty(&body.local_decls, tcx).ty;
    let layout = tcx
        .layout_of(PseudoCanonicalInput { typing_env, value: ty })
        .ok()?;
    // Enum payloads live in the variant layout, not the tag layout.
    // Guarded to real ADTs: for_variant bugs on non-ADT types.
    let layout = match kind {
        AggregateKind::Adt(_, variant_idx, ..) if matches!(ty.kind(), TyKind::Adt(..)) => {
            let cx = LayoutCx::new(tcx, typing_env);
            layout.for_variant(&cx, *variant_idx)
        }
        _ => layout,
    };
    // Primitive has no fields (offset() would panic); unions overlap all
    // fields at 0, which the alloc+store model cannot express — both fall
    // back to the v1 heuristic downstream.
    if !matches!(
        layout.fields,
        rustc_abi::FieldsShape::Arbitrary { .. } | rustc_abi::FieldsShape::Array { .. }
    ) {
        return None;
    }
    if layout.fields.count() != n {
        return None;
    }
    let mut offsets = Vec::with_capacity(n);
    for i in 0..n {
        offsets.push(layout.fields.offset(i).bytes());
    }
    Some((layout.size.bytes(), offsets))
}

fn body_json<'tcx>(
    tcx: TyCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    name: &str,
    body: &Body<'tcx>,
    into: &mut String,
) {
    into.push_str("{\"name\": \"");
    esc(name, into);
    into.push_str("\", \"locals\": [], \"blocks\": [");
    for (bi, data) in body.basic_blocks.iter_enumerated() {
        if bi.index() > 0 {
            into.push_str(", ");
        }
        into.push_str("{\"id\": \"");
        into.push_str(&bb_name(bi));
        into.push_str("\", \"statements\": [");
        let mut first = true;
        for st in &data.statements {
            if !first {
                into.push_str(", ");
            }
            first = false;
            match &st.kind {
                StatementKind::Assign(boxed) => {
                    let (place, rv) = &**boxed;
                    let (short, full) = place_name(place);
                    into.push_str("{\"kind\": \"Assign\", \"dest\": \"");
                    into.push_str(&short);
                    into.push_str("\", \"dest_place\": \"");
                    esc(&full, into);
                    into.push_str("\", \"rvalue\": ");
                    // p_layout v2 goes only on multi-operand aggregates.
                    let layout = match rv {
                        Rvalue::Aggregate(kind, ops) if ops.len() > 1 => {
                            aggregate_layout(tcx, param_env, body, kind, place, ops.len())
                        }
                        _ => None,
                    };
                    rvalue_json(rv, tcx, layout.as_ref(), into);
                    into.push('}');
                }
                StatementKind::StorageLive(l) => {
                    write!(into, "{{\"kind\": \"StorageLive\", \"local\": \"{}\"}}", local_name(*l)).unwrap();
                }
                StatementKind::StorageDead(l) => {
                    write!(into, "{{\"kind\": \"StorageDead\", \"local\": \"{}\"}}", local_name(*l)).unwrap();
                }
                // Runtime nops (analysis/coverage/counter only): exact kind,
                // backend renders a comment and does NOT count them.
                other @ (StatementKind::Nop | StatementKind::ConstEvalCounter | StatementKind::Coverage(_)) => {
                    into.push_str("{\"kind\": \"Nop\", \"text\": \"");
                    esc(&trunc(format!("{:?}", std::mem::discriminant(other)), 40), into);
                    into.push_str("\"}");
                }
                StatementKind::SetDiscriminant { place, variant_index } => {
                    let (s, full) = place_name(place);
                    write!(into, "{{\"kind\": \"SetDisc\", \"place\": \"{}\", \"place_full\": \"", s).unwrap();
                    esc(&full, into);
                    write!(into, "\", \"variant\": {}}}", variant_index.as_u32()).unwrap();
                }
                other => {
                    into.push_str("{\"kind\": \"UnsupportedStmt\", \"text\": \"");
                    esc(&trunc(format!("{:?}", other), 120), into);
                    into.push_str("\"}");
                }
            }
        }
        into.push_str("], \"terminator\": ");
        let t = match &data.terminator {
            Some(t) => &t.kind,
            None => {
                into.push_str("{\"kind\": \"Return\"}");
                into.push_str("}");
                continue;
            }
        };
        match t {
            TerminatorKind::Goto { target } => {
                write!(into, "{{\"kind\": \"Goto\", \"target\": \"{}\"}}", bb_name(*target)).unwrap();
            }
            TerminatorKind::SwitchInt { discr, targets } => {
                into.push_str("{\"kind\": \"SwitchInt\", \"discr\": ");
                operand_json(discr, tcx, into);
                into.push_str(", \"targets\": [");
                let mut fi = true;
                for (v, t) in targets.iter() {
                    if !fi {
                        into.push_str(", ");
                    }
                    fi = false;
                    write!(into, "[\"{}\", \"{}\"]", v, bb_name(t)).unwrap();
                }
                write!(into, "], \"otherwise\": \"{}\"}}", bb_name(targets.otherwise())).unwrap();
            }
            TerminatorKind::Return => into.push_str("{\"kind\": \"Return\"}"),
            TerminatorKind::Unreachable => into.push_str("{\"kind\": \"Unreachable\"}"),
            TerminatorKind::UnwindResume => into.push_str("{\"kind\": \"Resume\"}"),
            TerminatorKind::FalseEdge { real_target, .. } => {
                write!(into, "{{\"kind\": \"Goto\", \"target\": \"{}\"}}", bb_name(*real_target)).unwrap();
            }
            TerminatorKind::FalseUnwind { real_target, .. } => {
                write!(into, "{{\"kind\": \"Goto\", \"target\": \"{}\"}}", bb_name(*real_target)).unwrap();
            }
            TerminatorKind::Drop { place, target, .. } => {
                let (s, _) = place_name(place);
                write!(into, "{{\"kind\": \"Drop\", \"place\": \"{}\", \"target\": \"{}\"}}", s, bb_name(*target)).unwrap();
            }
            TerminatorKind::Call { func, args, destination, target, .. } => {
                let (fname, fraw) = fn_operand_name(tcx, func);
                into.push_str("{\"kind\": \"Call\", \"func\": \"");
                esc(&fname, into);
                into.push_str("\", \"func_raw\": \"");
                esc(&fraw, into);
                into.push_str("\", \"args\": [");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        into.push_str(", ");
                    }
                    operand_json(&a.node, tcx, into);
                }
                into.push(']');
                let (ds, _) = place_name(destination);
                write!(into, ", \"dest\": \"{}\"", ds).unwrap();
                match target {
                    Some(t) => write!(into, ", \"target\": \"{}\"", bb_name(*t)).unwrap(),
                    None => into.push_str(", \"target\": null"),
                }
                into.push('}');
            }
            TerminatorKind::Assert { cond, target, msg, .. } => {
                into.push_str("{\"kind\": \"Assert\", \"cond\": ");
                operand_json(cond, tcx, into);
                into.push_str(", \"target\": \"");
                into.push_str(&bb_name(*target));
                into.push_str("\", \"msg\": \"");
                esc(&trunc(format!("{:?}", msg), 80), into);
                into.push_str("\"}");
            }
            TerminatorKind::InlineAsm { template, operands, options, .. } => {
                // v2 structured capture (spans stripped for hermetic fixtures).
                // mir2sa gates exact patterns on these fields (`mov` reg-copy,
                // comment-only inout passthrough); everything else stays loud.
                let mut joined = String::new();
                let mut modifiers = false;
                for p in template.iter() {
                    match p {
                        InlineAsmTemplatePiece::String(s) => joined.push_str(s),
                        InlineAsmTemplatePiece::Placeholder { operand_idx, modifier, .. } => {
                            if modifier.is_some() {
                                modifiers = true;
                            }
                            write!(joined, "{{{}}}", operand_idx).unwrap();
                        }
                    }
                }
                let mut outs: Vec<String> = vec![];
                let mut ins: Vec<String> = vec![];
                let mut inout = false;
                for op in operands.iter() {
                    match op {
                        InlineAsmOperand::Out { place: Some(p), .. } => {
                            outs.push(place_name(p).0);
                        }
                        InlineAsmOperand::Out { place: None, .. } => {
                            outs.push("_proj".to_string());
                        }
                        InlineAsmOperand::In { value, .. } => {
                            let mut tmp = String::new();
                            operand_json(value, tcx, &mut tmp);
                            ins.push(tmp);
                        }
                        InlineAsmOperand::InOut { in_value, out_place, .. } => {
                            // Single-register passthrough (sla-117 shape): the
                            // value flows in and back out through one reg.
                            inout = true;
                            match out_place {
                                Some(p) => outs.push(place_name(p).0),
                                None => outs.push("_proj".to_string()),
                            }
                            let mut tmp = String::new();
                            operand_json(in_value, tcx, &mut tmp);
                            ins.push(tmp);
                        }
                        InlineAsmOperand::Const { .. } => {
                            ins.push("{\"kind\": \"Const\", \"value\": \"asm-operand-const\"}".to_string());
                        }
                        InlineAsmOperand::SymFn { .. } => {
                            ins.push("{\"kind\": \"Const\", \"value\": \"asm-operand-symfn\"}".to_string());
                        }
                        InlineAsmOperand::SymStatic { .. } => {
                            ins.push("{\"kind\": \"Const\", \"value\": \"asm-operand-symstatic\"}".to_string());
                        }
                        InlineAsmOperand::Label { .. } => {
                            ins.push("{\"kind\": \"Const\", \"value\": \"asm-operand-label\"}".to_string());
                        }
                    }
                }
                into.push_str("{\"kind\": \"InlineAsm\", \"text\": \"");
                esc(&trunc(format!("asm {:?} operands={}", template, operands.len()), 120), into);
                into.push_str("\", \"template\": \"");
                esc(&trunc(joined, 120), into);
                into.push_str("\", \"options\": \"");
                esc(&trunc(format!("{:?}", options), 80), into);
                into.push_str(&format!("\", \"modifiers\": {}, \"inout\": {}, \"outs\": [", modifiers, inout));
                for (i, o) in outs.iter().enumerate() {
                    if i > 0 {
                        into.push_str(", ");
                    }
                    write!(into, "\"{}\"", o).unwrap();
                }
                into.push_str("], \"ins\": [");
                for (i, s) in ins.iter().enumerate() {
                    if i > 0 {
                        into.push_str(", ");
                    }
                    into.push_str(s);
                }
                into.push_str("]}");
            }
            other => {
                into.push_str("{\"kind\": \"Unsupported\", \"text\": \"");
                esc(&trunc(format!("{:?}", other), 120), into);
                into.push_str("\"}");
            }
        }
        into.push('}');
    }
    into.push_str("]}");
}

use rustc_middle::ty;

impl Callbacks for RscCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        if tcx.dcx().err_count() > 0 {
            eprintln!("rsc_driver: analysis had errors; refusing to emit mir.json");
            std::process::exit(1);
        }
        let mut out = String::from("{\"source\": \"rsc_driver (rustc_private, real MIR)\", \"functions\": [");
        let mut first = true;
        for owner in tcx.hir_body_owners() {
            let did = owner.to_def_id();
            // `optimized_mir` panics on constants ("do not use optimized_mir
            // for constants"): route consts/statics through `mir_for_ctfe`,
            // skip bodies with no MIR at all.
            let body = match tcx.def_kind(did) {
                DefKind::Const | DefKind::AssocConst | DefKind::AnonConst | DefKind::Static { .. } => {
                    tcx.mir_for_ctfe(did)
                }
                DefKind::GlobalAsm => continue,
                _ => tcx.optimized_mir(did),
            };
            let path = tcx.def_path_str(did);
            let short = path.rsplit("::").next().unwrap_or(&path);
            if !first {
                out.push_str(", ");
            }
            first = false;
            body_json(tcx, tcx.param_env(did), short, body, &mut out);
        }
        out.push_str("]}");
        let approx = out.matches("RuntimeChecks-unsupported").count();
        if let Err(e) = std::fs::write(&self.out, &out) {
            eprintln!("rsc_driver: write {}: {}", self.out, e);
            std::process::exit(1);
        }
        eprintln!("rsc_driver: wrote {} APPROX={}", self.out, approx);
        Compilation::Stop
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let mut out = String::from("out.mir.json");
    let mut rustc_args: Vec<String> = vec![raw.get(0).cloned().unwrap_or_else(|| "rsc_driver".to_string())];
    let mut i = 1;
    while i < raw.len() {
        if raw[i] == "--rsc-out" {
            i += 1;
            if let Some(v) = raw.get(i) {
                out = v.clone();
            }
        } else {
            rustc_args.push(raw[i].clone());
        }
        i += 1;
    }
    let mut cb = RscCallbacks { out };
    run_compiler(&rustc_args, &mut cb);
}