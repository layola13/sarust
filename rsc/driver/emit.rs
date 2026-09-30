//! rsc_driver::emit.
use crate::place::{place_name, place_via};
use crate::tyinfo::{aggregate_layout, fn_operand_sig, operand_ty_short, str_const_bytes, ty_sa_sig_opt};
use crate::util::{bb_name, esc, local_name, sanitize, trunc};
use rustc_ast::ast::InlineAsmTemplatePiece;
use rustc_middle::mir::{Body, BorrowKind, Const, ConstOperand, InlineAsmOperand, Local, Operand, Rvalue, StatementKind, TerminatorKind};
use rustc_middle::ty::{ConstKind, TyCtxt, TyKind};
use rustc_middle::ty;
use std::fmt::Write as _;

pub fn operand_json<'a>(op: &Operand<'a>, tcx: TyCtxt<'a>, into: &mut String) {
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

/// Short scalar name for a cast operand's type (int/uint/float/bool/char,

pub fn rvalue_json<'tcx>(
    rv: &Rvalue<'tcx>,
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
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
            // Zero-sized borrows need no storage (the value carries no
            // data); the backend binds a null marker instead of a borrow.
            if crate::tyinfo::place_is_zst(tcx, body, p) {
                into.push_str(", \"zst\": true");
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
        Rvalue::Cast(kind, op, ty) => {
            into.push_str("{\"kind\": \"Cast\", \"op\": ");
            operand_json(op, tcx, into);
            into.push_str(", \"ty\": \"");
            esc(&trunc(format!("{:?}", ty), 60), into);
            into.push_str("\", \"castkind\": \"");
            esc(&trunc(format!("{:?}", kind), 40), into);
            into.push_str("\", \"src_ty\": \"");
            esc(&operand_ty_short(tcx, body, op), into);
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

/// SA-level type name for signatures: scalars normalized (`bool`->`u8`,
/// `usize`->`u64`, `char`->`u32`), unit->`void`, everything else (`&`/`*`,
/// ADT, slices, tuples, fn) is an opaque `ptr` handle (existing sla

pub fn fn_operand_name(tcx: TyCtxt<'_>, op: &Operand<'_>) -> (String, String) {
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

pub fn body_json<'tcx>(
    tcx: TyCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    name: &str,
    body: &Body<'tcx>,
    into: &mut String,
) {
    into.push_str("{\"name\": \"");
    esc(name, into);
    into.push_str("\", \"locals\": [], ");
    // Callee-visible signature: MIR arg locals `_1..=_arg_count` plus the
    // return-place type, mapped to SA tys (opaque `ptr` fallback). The
    // backend declares typed params from these (bare `@f()` leaves MIR
    // params unbound, which is UnknownRegister).
    into.push_str("\"params\": [");
    let mut sig_ok = true;
    for i in 1..=body.arg_count {
        if i > 1 {
            into.push_str(", ");
        }
        let ty = body.local_decls[Local::from_u32(i as u32)].ty;
        match ty_sa_sig_opt(ty) {
            Some(t) => write!(into, "\"{}\"", t).unwrap(),
            None => {
                sig_ok = false;
                into.push_str("\"ptr\"");
            }
        }
    }
    let ret_ty = body.local_decls[Local::from_u32(0)].ty;
    let ret = ty_sa_sig_opt(ret_ty);
    if ret.is_none() {
        sig_ok = false;
    }
    write!(into, "], \"ret\": \"{}\"", ret.unwrap_or_else(|| "ptr".to_string())).unwrap();
    write!(into, ", \"sig_ok\": {}", sig_ok).unwrap();
    into.push_str(", \"blocks\": [");
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
                    rvalue_json(rv, tcx, body, layout.as_ref(), into);
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
                // Callee signature for typed `@extern` decls (bare `()` decls
                // mismatch any call with args: CapabilityMismatch). Absent
                // when unresolvable; the backend goes loud for such calls.
                match fn_operand_sig(tcx, func) {
                    Some((params, ret)) => {
                        into.push_str(", \"sig\": {\"params\": [");
                        for (i, p) in params.iter().enumerate() {
                            if i > 0 {
                                into.push_str(", ");
                            }
                            write!(into, "\"{}\"", p).unwrap();
                        }
                        write!(into, "], \"ret\": \"{}\"}}", ret).unwrap();
                    }
                    None => into.push_str(", \"sig\": null"),
                }
                let (ds, _) = place_name(destination);
                write!(into, ", \"dest\": \"{}\"", ds).unwrap();
                match target {
                    Some(t) => write!(into, ", \"target\": \"{}\"", bb_name(*t)).unwrap(),
                    None => into.push_str(", \"target\": null"),
                }
                into.push('}');
            }
            TerminatorKind::Assert { cond, target, msg, expected, .. } => {
                into.push_str("{\"kind\": \"Assert\", \"cond\": ");
                operand_json(cond, tcx, into);
                into.push_str(", \"target\": \"");
                into.push_str(&bb_name(*target));
                into.push_str("\", \"msg\": \"");
                esc(&trunc(format!("{:?}", msg), 80), into);
                write!(into, "\", \"expected\": {}", expected).unwrap();
                into.push_str("}");
            }
            TerminatorKind::InlineAsm { template, operands, options, targets, .. } => {
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
                into.push_str("]");
                // First target is the fallthrough destination (absent for
                // naked/noreturn asm). RPO block ordering needs it explicit:
                // SA has no implicit fallthrough across reordered blocks.
                match targets.first() {
                    Some(t) => write!(into, ", \"target\": \"{}\"", bb_name(*t)).unwrap(),
                    None => into.push_str(", \"target\": null"),
                }
                into.push('}');
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
