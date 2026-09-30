//! mir2sa: MIR -> SA backend in pure Rust.
//!
//! Subcommands (1:1 replacements of the retired Python prototypes):
//!   mir2sa parse <mir-text> --fn <name> --out <mir.json>   # real `-Zunpretty=mir` -> mir.json
//!   mir2sa lower <mir.json> --out <out.sa> [--strict]      # mir.json -> .sa
//!
//! Mapping (identical to the tutorial + retired prototype):
//!   Operand::Move(p)  -> ^p        Operand::Copy(p) -> p
//!   Rvalue::Ref       -> &p        Terminator::Drop(p) -> !p

use serde::{Deserialize, Serialize};
use std::process::ExitCode;

// ---------------------------------------------------------------------------
// mir.json schema (same keys as the retired prototype so old JSON keeps working)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct MirFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    functions: Vec<Function>,
}

#[derive(Serialize, Deserialize)]
struct Function {
    name: String,
    #[serde(default)]
    locals: Vec<serde_json::Value>,
    blocks: Vec<Block>,
}

#[derive(Serialize, Deserialize)]
struct Block {
    id: String,
    #[serde(default)]
    statements: Vec<Stmt>,
    #[serde(default = "default_return")]
    terminator: Term,
}

fn default_return() -> Term {
    Term::Return
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum Stmt {
    Assign { dest: String, #[serde(default)] dest_place: Option<String>, rvalue: Rvalue },
    StorageLive { local: String },
    StorageDead { local: String },
    Nop { text: String },
    SetDisc { place: String, variant: u32, #[serde(default, skip_serializing_if = "Option::is_none")] place_full: Option<String> },
    UnsupportedStmt { text: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum Rvalue {
    Use { op: Operand },
    Ref { place: String, #[serde(rename = "mut", default, skip_serializing_if = "is_false")] mut_: bool, #[serde(default, skip_serializing_if = "Option::is_none")] via: Option<String> },
    Call { func: String, #[serde(default)] args: Vec<Operand> },
    BinOp { op: String, left: Box<Operand>, right: Box<Operand> },
    UnOp { op: String, operand: Box<Operand> },
    Cast { op: Box<Operand>, ty: String },
    Aggregate { elems: Vec<Operand> },
    Discriminant { place: String },
    RawPtr { place: String, #[serde(rename = "mut", default, skip_serializing_if = "is_false")] mut_: bool },
    Repeat { op: Box<Operand>, len: String },
    ThreadLocal { def: String },
    Unsupported { text: String },
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
enum Operand {
    Move { place: String },
    Copy { place: String },
    Const { value: String },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
enum Term {
    Goto { target: String },
    Return,
    Resume,
    Call {
        func: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        func_raw: Option<String>,
        #[serde(default)]
        args: Vec<Operand>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dest: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    Drop { place: String, target: String },
    SwitchInt { discr: Box<Operand>, #[serde(default)] targets: Vec<(String, String)>, otherwise: String },
    Assert { cond: Box<Operand>, target: String, #[serde(default, skip_serializing_if = "Option::is_none")] msg: Option<String> },
    Unreachable,
    InlineAsm { text: String },
    Unsupported { text: String },
}

fn is_false(b: &bool) -> bool {
    !b
}

// ---------------------------------------------------------------------------
// lower: mir.json -> .sa
// ---------------------------------------------------------------------------

fn render_operand(op: &Operand) -> String {
    match op {
        Operand::Move { place } => format!("^{}", place),
        Operand::Copy { place } => place.clone(),
        Operand::Const { value } => value.clone(),
    }
}

fn array_elem_size(ty: &str) -> Option<usize> {
    match ty {
        "i8" | "u8" | "bool" => Some(1),
        "i32" | "u32" | "f32" => Some(4),
        "i64" | "u64" | "f64" | "usize" | "isize" => Some(8),
        _ => None,
    }
}

fn sa_scalar_ty(ty: &str) -> &str {
    match ty {
        "bool" => "u8",
        "usize" => "u64",
        "isize" => "i64",
        t => t,
    }
}

/// One array-init element to decimal text. Accepts both the `-Zunpretty` text
/// form (`1_i32`, `true`) and the driver's Debug form
/// (`Val(Scalar(0x00000001), i32)`). Anything else -> None (stay UNSUPPORTED).
fn const_array_elem(value: &str, ty: &str, size: usize) -> Option<String> {
    if ty == "bool" {
        return match value {
            "true" => Some("1".to_string()),
            "false" => Some("0".to_string()),
            _ => scalar_hex_value(value, "bool").map(|n| (n != 0).to_string()),
        };
    }
    if let Some(us) = value.rfind('_') {
        let (num, suf) = (&value[..us], &value[us + 1..]);
        if suf == ty
            && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit() || c == '-')
            && num.matches('-').count() <= 1
            && (!num.contains('-') || num.starts_with('-'))
        {
            return Some(num.to_string());
        }
        return None;
    }
    scalar_hex_value(value, ty).map(|n| {
        // Two's-complement wrap for signed types (e.g. 0xFFFFFFFF -> -1).
        let bits = size * 8;
        if matches!(ty, "i8" | "i16" | "i32" | "i64" | "isize") && bits < 128 && (n >> (bits - 1)) & 1 == 1 {
            ((n as i128).wrapping_sub(1 << bits)).to_string()
        } else {
            n.to_string()
        }
    })
}

/// `Val(Scalar(0xHEX), TY)` -> integer iff TY matches; None otherwise.
fn scalar_hex_value(value: &str, ty: &str) -> Option<u128> {
    let v = value.strip_prefix("Val(Scalar(0x")?;
    let comma = v.find(", ")?;
    let hex = v[..comma].trim_end_matches(')');
    let suffix = v[comma + 2..].trim_end_matches(')');
    if suffix != ty {
        return None;
    }
    // Guard: hex payload only, no nested Debug text.
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) || hex.is_empty() {
        return None;
    }
    u128::from_str_radix(hex, 16).ok()
}

/// `[elem; len]` const-fill plan -> (decimal byte value, total bytes).
/// Accepts text (`7_u8`, `8`) and driver-Debug (`Val(Scalar(0x07), u8)`)
/// shapes. Element must be a single byte value (u8/i8), like the
/// `sa_mem_set(&buf, 0, bytes)` sites in `sci/sa_std/alloc/vec.sa`.
fn repeat_plan(op_value: &str, len_text: &str) -> Option<(String, usize)> {
    let (val, size) = repeat_elem(op_value)?;
    let n = repeat_len(len_text)?;
    n.checked_mul(size).map(|total| (val, total))
}

fn repeat_elem(op_value: &str) -> Option<(String, usize)> {
    // Text form `7_u8`.
    if let Some(us) = op_value.rfind('_') {
        let (num, suf) = (&op_value[..us], &op_value[us + 1..]);
        if matches!(suf, "u8" | "i8") && !num.is_empty()
            && num.chars().all(|c| c.is_ascii_digit() || c == '-')
        {
            let v: i128 = num.parse().ok()?;
            return byte_value(v, suf).map(|b| (b.to_string(), 1));
        }
        return None;
    }
    // Driver-Debug form `Val(Scalar(0x07), u8)`.
    for ty in ["u8", "i8"] {
        if let Some(n) = scalar_hex_value(op_value, ty) {
            let v = if ty == "i8" && n >= 128 { (n as i128) - 256 } else { n as i128 };
            return byte_value(v, ty).map(|b| (b.to_string(), 1));
        }
    }
    None
}

fn byte_value(v: i128, ty: &str) -> Option<i128> {
    match ty {
        "u8" if (0..=255).contains(&v) => Some(v),
        "i8" if (-128..=127).contains(&v) => Some(v),
        _ => None,
    }
}

fn repeat_len(len_text: &str) -> Option<usize> {
    let t = len_text.trim();
    if let Ok(n) = t.parse::<usize>() {
        return Some(n);
    }
    // `<num>_<tysuffix>` (text `8`, pretty `8_usize`): digits before `_`.
    if let Some(us) = t.find('_') {
        if let Ok(n) = t[..us].parse::<usize>() {
            if t[us + 1..].trim_end_matches(')').chars().all(|c| c.is_ascii_alphanumeric()) {
                return Some(n);
            }
        }
    }
    // Driver-Debug scalar: `Val(Scalar(0x08), usize)`.
    scalar_hex_value(t, "usize").and_then(|n| usize::try_from(n).ok())
}

/// `(((*_46).1: ...).0: [i32; 3])` lowers to sla-style `alloc` + `store`
/// (cf. `sci/sa_std/alloc/vec.sa`: `buf = alloc bytes`,
/// `store base+off, v as u64`). Returns SA lines or None (caller keeps it
/// UNSUPPORTED: non-const elems, unknown layout, count mismatch).
fn lower_array_init(bid: &str, dest_place: &str, elems: &[Operand]) -> Option<Vec<String>> {
    let lb = dest_place.rfind('[')?;
    let rb = dest_place[lb..].find(']')? + lb;
    let inner = &dest_place[lb + 1..rb];
    let mut parts = inner.split(';');
    let ty = parts.next()?.trim();
    let n: usize = parts.next()?.trim().parse().ok()?;
    if parts.next().is_some() || n != elems.len() {
        return None;
    }
    let size = array_elem_size(ty)?;
    let sa_ty = sa_scalar_ty(ty);
    let mut vals = Vec::with_capacity(elems.len());
    for e in elems {
        match e {
            Operand::Const { value } => {
                vals.push(const_array_elem(value, ty, size)?);
            }
            _ => return None, // Moves/Copies must stay visible; never hide them in a store.
        }
    }
    let base = format!("_agg_{}", bid);
    let mut lines = vec![
        format!("// aggregate {} init {}", dest_place.trim(), inner.trim()),
        format!("{} = alloc {}", base, n * size),
    ];
    for (i, v) in vals.iter().enumerate() {
        lines.push(format!("store {}+{}, {} as {}", base, i * size, v, sa_ty));
    }
    Some(lines)
}

fn align_agg_offset(offset: usize, size: usize) -> usize {
    // Mirrors sla alignAggregateOffset: only 8-byte fields force alignment.
    if size == 8 { (offset + 7) & !7 } else { offset }
}

/// Infer `(ty, size)` for a Const aggregate element.
/// Accepts text form (`1_i32`) and driver-Debug form
/// (`Val(Scalar(0x00000001), i32)`); bool true/false included.
fn const_elem_ty(value: &str) -> Option<(&'static str, usize)> {
    let v = value.trim();
    if v == "true" || v == "false" {
        return Some(("bool", 1));
    }
    if let Some(us) = v.rfind('_') {
        let suf = &v[us + 1..];
        // Text form suffix is a bare type name (no parens/commas/spaces).
        if !suf.is_empty()
            && suf.chars().all(|c| c.is_ascii_alphanumeric())
            && !v[..us].is_empty()
        {
            let size = match suf {
                "bool" | "u8" | "i8" => 1,
                "u16" | "i16" => 2,
                "u32" | "i32" | "f32" => 4,
                "u64" | "i64" | "usize" | "isize" | "f64" => 8,
                _ => return None,
            };
            return Some((match suf {
                "bool" => "bool", "u8" => "u8", "i8" => "i8",
                "u16" => "u16", "i16" => "i16",
                "u32" => "u32", "i32" => "i32", "f32" => "f32",
                "u64" => "u64", "i64" => "i64", "usize" => "usize",
                "isize" => "isize", "f64" => "f64",
                _ => return None,
            }, size));
        }
    }
    // Driver-Debug form: `Val(Scalar(0xHEX), TY)`.
    if let Some(rest) = v.strip_prefix("Val(Scalar(0x") {
        if let Some(comma) = rest.find(", ") {
            let suffix = rest[comma + 2..].trim_end_matches(')');
            let size = match suffix {
                "bool" | "u8" | "i8" => 1,
                "u16" | "i16" => 2,
                "u32" | "i32" | "f32" => 4,
                "u64" | "i64" | "usize" | "isize" | "f64" => 8,
                _ => return None,
            };
            // Leak a 'static str would be wrong; return size only via caller map.
            // Instead match again to a static slice (suffix is not 'static).
            let ty: &'static str = match suffix {
                "bool" => "bool", "u8" => "u8", "i8" => "i8",
                "u16" => "u16", "i16" => "i16",
                "u32" => "u32", "i32" => "i32", "f32" => "f32",
                "u64" => "u64", "i64" => "i64", "usize" => "usize",
                "isize" => "isize", "f64" => "f64",
                _ => return None,
            };
            return Some((ty, size));
        }
    }
    // `Val(Scalar(0x01), bool)` single-hex bool is covered above via suffix;
    // bare `0`/`1` without suffix cannot be typed -> caller keeps UNSUPPORTED.
    None
}

/// Generic Adt (struct/tuple/range/enum-payload) lowering to sla-style
/// `alloc` + per-field `store` (cf. `sci/sa_std/alloc/vec.sa`).
/// Layout mirrors `sa_plugin_sla` tuple/struct ABI (packed except 8-byte
/// alignment). Move elements stay visible as `^p`; Const elements are
/// decimalized; unknown-typed Moves/Copies default to 8-byte `u64` slots
/// (sla `else => 8`). Returns SA lines bound to `dest`, or None.
fn lower_adt_init(bid: &str, dest: &str, dest_place: &str, elems: &[Operand]) -> Option<Vec<String>> {
    if elems.len() < 2 {
        return None;
    }
    let mut plans: Vec<(String, &'static str, usize)> = Vec::with_capacity(elems.len());
    for e in elems {
        match e {
            Operand::Const { value } => {
                let (ty, size) = const_elem_ty(value)?;
                let rendered = match ty {
                    "bool" => {
                        if value.trim() == "true" {
                            "1".to_string()
                        } else if value.trim() == "false" {
                            "0".to_string()
                        } else {
                            let n = scalar_hex_value(value, "bool")?;
                            ((n != 0) as u8).to_string()
                        }
                    }
                    _ => const_array_elem(value, ty, size)?,
                };
                plans.push((rendered, sa_scalar_ty(ty), size));
            }
            Operand::Move { place } => {
                plans.push((format!("^{}", place), "u64", 8));
            }
            Operand::Copy { place } => {
                plans.push((place.clone(), "u64", 8));
            }
        }
    }
    let mut offsets = Vec::with_capacity(plans.len());
    let mut off = 0usize;
    for (_, _, size) in &plans {
        off = align_agg_offset(off, *size);
        offsets.push(off);
        off += *size;
    }
    let total = off.max(1);
    let base = format!("_agg_{}", bid);
    let mut lines = vec![
        format!("// aggregate Adt init @ {} (p_layout v1: sla tuple/struct ABI)", dest_place.trim()),
        format!("{} = alloc {}", base, total),
    ];
    for (i, (rendered, sa_ty, _)) in plans.iter().enumerate() {
        lines.push(format!("store {}+{}, {} as {}", base, offsets[i], rendered, sa_ty));
    }
    lines.push(format!("{} = {}", dest, base));
    Some(lines)
}

/// FNV-1a 64 of a ThreadLocal def path: the u64 key passed to
/// `sa_thread_local_slot` (see `sci/sa_std/thread_local.sai`). Keeps .sa
/// string-free; collisions across a crate's few TLS statics are impractical.
fn tls_key(def: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in def.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn render_rvalue(
    rv: &Rvalue,
    dest: &str,
    dest_place: Option<&str>,
    unsup: &mut Vec<String>,
    bid: &str,
) -> String {
    match rv {
        Rvalue::Use { op } => format!("{} = {}", dest, render_operand(op)),
        Rvalue::Ref { place, mut_, via } => {
            let mut s = if *mut_ {
                format!("{} = &{} // mut-borrow (Phase1: & + Referee)", dest, place)
            } else {
                format!("{} = &{}", dest, place)
            };
            if let Some(v) = via {
                s += &format!(" // via {}", v);
            }
            s
        }
        Rvalue::Call { func, args } => {
            let a: Vec<String> = args.iter().map(render_operand).collect();
            format!("{} = call @{}({})", dest, func, a.join(", "))
        }
        Rvalue::BinOp { op, left, right } => format!(
            "{} = {}({}, {})",
            dest,
            op,
            render_operand(left),
            render_operand(right)
        ),
        Rvalue::UnOp { op, operand } => format!("{} = {}({})", dest, op, render_operand(operand)),
        Rvalue::Cast { op, ty } => format!("{} = *{} // cast: {}", dest, render_operand(op), ty),
        Rvalue::Discriminant { place } => format!("{} = discriminant({}) // enum tag read", dest, place),
        Rvalue::RawPtr { place, mut_ } => {
            if *mut_ {
                format!("{} = *{} // raw-ptr mut (assume_safe; Referee: UnsafeBinder)", dest, place)
            } else {
                format!("{} = *{} // raw-ptr const", dest, place)
            }
        }
        Rvalue::Repeat { op, len } => {
            match op.as_ref() {
                Operand::Const { value } => match repeat_plan(value, len) {
                    Some((v, total)) => format!(
                        "// repeat [{}; {}]\n_rep_{} = alloc {}\ncall @sa_mem_set(&_rep_{}, {}, {})",
                        value, len, bid, total, bid, v, total
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
            // Lowered to the shared registry (sci/sa_std/thread_local.sai):
            // per-thread u64 cell, zero-init. No std copy in rsc, only the
            // DefPath -> key mapping (STD_MAP.md).
            format!("{} = call @sa_thread_local_slot({}) // thread-local: {}", dest, tls_key(def), def)
        }
        Rvalue::Aggregate { elems } => {
            if elems.is_empty() {
                format!("{} = 0 // zero-elem aggregate (unit/niche; tag via SetDisc when present)", dest)
            } else if elems.len() == 1 {
                format!("{} = {} // single-elem aggregate (exact)", dest, render_operand(&elems[0]))
            } else if let Some(place) = dest_place {
                // Array fast path first (exact [T; N] all-const, incl. ManuallyDrop backing).
                if let Some(lines) = lower_array_init(bid, place, elems) {
                    lines.join("\n")
                } else if let Some(lines) = lower_adt_init(bid, dest, place, elems) {
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

fn lower_function(f: &Function, unsup: &mut Vec<String>) -> String {
    let mut out = vec![format!("@{}() -> i32:", f.name)];
    for b in &f.blocks {
        out.push(format!("{}:", b.id));
        for st in &b.statements {
            match st {
                Stmt::Assign { dest, dest_place, rvalue } => {
                    let line = render_rvalue(rvalue, dest, dest_place.as_deref(), unsup, &b.id);
                    if line.contains('\n') {
                        for l in line.split('\n') {
                            out.push(format!("    {}", l));
                        }
                    } else {
                        let mut l = line;
                        if dest_place.as_deref() != Some(dest.as_str()) {
                            if let Some(dp) = dest_place {
                                l += &format!(" // place: {}", dp);
                            }
                        }
                        out.push(format!("    {}", l));
                    }
                }
                Stmt::StorageLive { local } => out.push(format!("    // StorageLive {}", local)),
                Stmt::StorageDead { local } => out.push(format!("    // StorageDead {}", local)),
                Stmt::Nop { text } => out.push(format!("    // nop: {}", text)),
                Stmt::SetDisc { place, variant, .. } => {
                    // p_layout v1: enum tag lives at offset 0 (sla enum_tag_offset=0,
                    // payload at 8). Driver reports the base local already; the
                    // full place (`(*_9)`) is kept as a comment for provenance.
                    out.push(format!("    store {}+0, {} as i64 // set-discriminant (enum tag)", place, variant));
                }
                Stmt::UnsupportedStmt { text } => {
                    unsup.push(format!("{}: UnsupportedStmt", b.id));
                    out.push(format!("    // UNSUPPORTED stmt: {}", text));
                }
            }
        }
        match &b.terminator {
            Term::Goto { target } => out.push(format!("    jmp {}", target)),
            Term::Return => out.push("    return 0".to_string()),
            Term::Resume => out.push("    panic(\"unwind-resume\") // MIR Resume (cleanup path)".to_string()),
            Term::Call { func, func_raw, args, dest, target } => {
                if let Some(raw) = func_raw {
                    out.push(format!("    // MIR: {}", raw));
                }
                let a: Vec<String> = args.iter().map(render_operand).collect();
                out.push(format!("    {} = call @{}({})", dest.as_deref().unwrap_or("_0"), func, a.join(", ")));
                match target {
                    Some(t) => out.push(format!("    jmp {}", t)),
                    None => out.push("    unreachable // diverging call".to_string()),
                }
            }
            Term::Drop { place, target } => {
                out.push(format!("    !{}", place));
                out.push(format!("    jmp {}", target));
            }
            Term::SwitchInt { discr, targets, otherwise } => {
                // Single-move fan-out: a Move discriminant must be bound once,
                // otherwise every `br ^p` arm would move `p` again.
                let d = match discr.as_ref() {
                    Operand::Move { place } => {
                        let tmp = format!("_sw_{}", b.id);
                        out.push(format!("    {} = ^{}", tmp, place));
                        tmp
                    }
                    _ => render_operand(discr),
                };
                for (v, t) in targets {
                    out.push(format!("    br {} == {} -> {}", d, v, t));
                }
                out.push(format!("    jmp {}", otherwise));
            }
            Term::Assert { cond, target, .. } => {
                out.push(format!("    assert {}", render_operand(cond)));
                out.push(format!("    jmp {}", target));
            }
            Term::Unreachable => out.push("    unreachable".to_string()),
            Term::InlineAsm { text } => {
                unsup.push(format!("{}: InlineAsm", b.id));
                out.push(format!("    // UNSUPPORTED inline-asm: {} (no SA equivalent; extern/intrinsic TBD)", text));
            }
            Term::Unsupported { text } => {
                unsup.push(format!("{}: UnsupportedTerm", b.id));
                out.push(format!("    // UNSUPPORTED terminator: {}", text));
            }
        }
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

fn collect_externs(mir: &MirFile) -> std::collections::BTreeSet<String> {
    // sla convention (cf. sa_plugin_sla emitExternDecl): every called-but-
    // undefined symbol gets an `@extern` decl so the module is closed.
    // Signatures are unknown at MIR level; map them to sci/sa_std per STD_MAP.md.
    let mut set = std::collections::BTreeSet::new();
    for f in &mir.functions {
        for b in &f.blocks {
            for st in &b.statements {
                if let Stmt::Assign { rvalue: Rvalue::Call { func, .. }, .. } = st {
                    set.insert(func.clone());
                }
            }
            if let Term::Call { func, .. } = &b.terminator {
                set.insert(func.clone());
            }
        }
    }
    set.into_iter().collect::<std::collections::BTreeSet<String>>()
}

fn cmd_lower(args: &[String]) -> ExitCode {
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
    let mut unsup = vec![];
    // Bodies joined by "\n"; each body already ends with one trailing newline.
    // sla convention: `@extern` decls for all called-but-undefined symbols.
    let mut sa_compat = String::from("@import \"sa_std/io/print.sai\"\n\n");
    let mut exts = collect_externs(&mir);
    let bodies: Vec<String> = mir.functions.iter().map(|f| lower_function(f, &mut unsup)).collect();
    if bodies.iter().any(|b| b.contains("sa_mem_set")) {
        // Repeat lowering emits `call @sa_mem_set` directly (not via Rvalue::Call).
        exts.insert("sa_mem_set".to_string());
    }
    if bodies.iter().any(|b| b.contains("sa_thread_local_slot")) {
        // ThreadLocal lowering emits `call @sa_thread_local_slot` directly
        // (registry lives in sci/sa_std/thread_local.sai, not in rsc).
        exts.insert("sa_thread_local_slot".to_string());
    }
    if !exts.is_empty() {
        sa_compat += "// MIR callees (map to sci/sa_std per STD_MAP.md):\n";
        for e in &exts {
            sa_compat += &format!("@extern {}()\n", e);
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

// ---------------------------------------------------------------------------
// parse: real `-Zunpretty=mir` text -> mir.json
// ---------------------------------------------------------------------------

fn split_top(s: &str) -> Vec<String> {
    let (mut parts, mut depth, mut cur, mut q) = (vec![], 0i32, String::new(), None::<char>);
    for ch in s.chars() {
        if let Some(qq) = q {
            cur.push(ch);
            if ch == qq { q = None; }
            continue;
        }
        match ch {
            '"' | '\'' => { q = Some(ch); cur.push(ch); }
            '<' | '(' | '[' => { depth += 1; cur.push(ch); }
            '>' | ')' | ']' => { depth -= 1; cur.push(ch); }
            ',' if depth == 0 => { parts.push(cur.trim().to_string()); cur = String::new(); }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts
}

fn base_local(place: &str) -> Option<String> {
    // Strip projections/wrappers (`(_4.0: T)`, `(_7 as Some).0`) down to the
    // base local, so SA places stay plain `_N`.
    let p = place.trim().trim_start_matches('(').trim();
    if p.starts_with('_') && p[1..].chars().take_while(|c| c.is_ascii_digit()).count() > 0 {
        let n: String = p[1..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if p.len() == 1 + n.len() {
            return Some(format!("_{}", n));
        }
        if p[1 + n.len()..].starts_with(|c| c == '.' || c == ':' || c == ' ') {
            return Some(format!("_{}", n));
        }
    }
    // fall back: leading _digits token
    let mut it = p.chars();
    if it.next() != Some('_') { return None; }
    let n: String = it.take_while(|c| c.is_ascii_digit()).collect();
    if n.is_empty() { None } else { Some(format!("_{}", n)) }
}

fn parse_operand(s: &str) -> Operand {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("move ") {
        return Operand::Move { place: base_local(rest).unwrap_or_else(|| rest.to_string()) };
    }
    if let Some(rest) = s.strip_prefix("copy ") {
        return Operand::Copy { place: base_local(rest).unwrap_or_else(|| rest.to_string()) };
    }
    if let Some(rest) = s.strip_prefix("const ") {
        return Operand::Const { value: rest.chars().take(60).collect() };
    }
    if s.starts_with('_') && base_local(s).as_deref() == Some(s) {
        return Operand::Copy { place: s.to_string() };
    }
    let mut v: String = s.chars().take(60).collect();
    Operand::Const { value: v }
}

fn sanitize_func(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // strip ::<'...> and ::<...> turbofish
    loop {
        let start = match s.find("::<") {
            Some(i) => i,
            None => break,
        };
        let mut depth = 0;
        let mut end = None;
        for (j, ch) in s[start + 3..].char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => { if depth == 0 { end = Some(start + 3 + j); break; } else { depth -= 1; } }
                _ => {}
            }
        }
        match end {
            Some(e) => { s.replace_range(start..=e, ""); }
            None => break,
        }
    }
    s = s.replace(" as ", "_as_").replace('&', "");
    // Collapse runs of separators into one `_` (same as Python
    // `re.sub(r"[^A-Za-z0-9_]+", "_", s)`), then trim edge `_`.
    let mut clean = String::new();
    let mut sep = true; // leading separators trimmed by collapsing
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            clean.push(c);
            sep = false;
        } else if !sep {
            clean.push('_');
            sep = true;
        }
    }
    while clean.ends_with('_') {
        clean.pop();
    }
    clean.chars().take(80).collect::<String>()
}

const BINOPS: &[&str] = &["Gt", "Eq", "Ne", "Lt", "Le", "Ge", "Add", "Sub", "Mul", "Div", "Rem",
    "BitAnd", "BitOr", "BitXor", "Shl", "Shr", "Offset"];

fn parse_call(line: &str) -> Option<(Option<String>, String, String, Vec<Operand>, Option<String>)> {
    // `DEST = FUNC(ARGS) -> [META];` ; terminator shapes excluded by caller
    let arrow = line.rfind("-> [")?;
    let (left, meta) = (&line[..arrow], &line[arrow + 4..]);
    let left = left.trim().strip_suffix(';').unwrap_or(left).trim();
    let (dest, call) = match left.find('=') {
        Some(i) => {
            let (d, c) = (&left[..i], &left[i + 1..]);
            (Some(d.trim().to_string()), c.trim())
        }
        None => (None, left),
    };
    // guard: not a drop/switchInt/assert/goto line (caller handles those first,
    // but keep the guard for safety)
    let head = call.split('(').next().unwrap_or("").trim();
    if ["drop", "switchInt", "assert", "goto"].contains(&head) {
        return None;
    }
    let paren = call.find('(')?;
    let func = call[..paren].trim().to_string();
    let args_str = call[paren + 1..].rsplit(')').nth(1).unwrap_or("").trim();
    // NOTE: rsplit(')') takes text before the LAST ')'; meta was already split
    // off, so this is the args region (may itself contain balanced parens,
    // which split_top respects).
    let args = if args_str.is_empty() { vec![] } else { split_top(args_str).into_iter().map(|a| parse_operand(&a)).collect() };
    let target = meta.split(',').find_map(|kv| {
        let kv = kv.trim().trim_end_matches(|c| c == ']' || c == ';');
        kv.strip_prefix("return:").map(|v| v.trim().to_string())
    });
    Some((dest, sanitize_func(&func), func, args, target))
}

fn parse_rvalue(rhs: &str) -> Rvalue {
    let rhs = rhs.trim();
    if let Some(rest) = rhs.strip_prefix("&mut ") {
        let p = rest.trim();
        return Rvalue::Ref { place: base_local(p).unwrap_or_else(|| p.to_string()), mut_: true, via: None };
    }
    if let Some(rest) = rhs.strip_prefix('&') {
        let p = rest.trim();
        return Rvalue::Ref { place: base_local(p).unwrap_or_else(|| p.to_string()), mut_: false, via: None };
    }
    // Cast before plain Use so `copy X as T (Kind)` is not swallowed.
    if let Some(as_pos) = rhs.find(" as ") {
        let tail = &rhs[as_pos + 4..];
        if let Some(kp) = tail.rfind('(') {
            if tail.ends_with(')') && kp > 0 && tail[..kp].trim().len() > 0
                && tail[kp + 1..tail.len() - 1].chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                let ty = tail[..kp].trim();
                return Rvalue::Cast {
                    op: Box::new(parse_operand(rhs[..as_pos].trim())),
                    ty: ty.chars().take(60).collect(),
                };
            }
        }
    }
    if rhs.starts_with("move ") || rhs.starts_with("copy ") || rhs.starts_with("const ")
        || (rhs.starts_with('_') && base_local(rhs).as_deref() == Some(rhs))
    {
        return Rvalue::Use { op: parse_operand(rhs) };
    }
    if let Some(inner) = rhs.strip_prefix("no_retag copy ") {
        let inner = inner.trim();
        return Rvalue::Use { op: Operand::Copy { place: base_local(inner).unwrap_or_else(|| inner.chars().take(40).collect()) } };
    }
    if rhs.ends_with(')') {
        if let Some(pi) = rhs.find('(') {
            let name = &rhs[..pi];
            let inner = &rhs[pi + 1..rhs.len() - 1];
            if BINOPS.contains(&name) {
                let parts = split_top(inner);
                if parts.len() == 2 {
                    return Rvalue::BinOp { op: name.to_string(), left: Box::new(parse_operand(&parts[0])), right: Box::new(parse_operand(&parts[1])) };
                }
            }
            if name == "Not" {
                return Rvalue::UnOp { op: "Not".to_string(), operand: Box::new(parse_operand(inner)) };
            }
        }
    }
    if (rhs.starts_with('[') && rhs.ends_with(']')) || (rhs.starts_with('(') && rhs.ends_with(')')) {
        let inner = rhs[1..rhs.len() - 1].trim().trim_end_matches(',').trim();
        if !inner.is_empty() {
            return Rvalue::Aggregate { elems: split_top(inner).into_iter().map(|e| parse_operand(&e)).collect() };
        }
    }
    Rvalue::Unsupported { text: rhs.chars().take(120).collect() }
}

fn sanitize_dest(d: &str) -> String {
    let d = d.trim();
    if d.starts_with('_') && base_local(d).as_deref() == Some(d) {
        return d.to_string();
    }
    base_local(d).unwrap_or_else(|| "_proj".to_string())
}

fn find_id(s: &str, key: &str) -> Option<String> {
    // find `key: bbN` inside bracket meta
    for kv in s.split(',') {
        let kv = kv.trim().trim_end_matches(|c| c == ']' || c == ';');
        if let Some(v) = kv.strip_prefix(key) {
            let v = v.trim().trim_start_matches(':').trim();
            if v.starts_with("bb") {
                return Some(v.chars().take_while(|c| c.is_alphanumeric()).collect());
            }
        }
    }
    None
}

fn parse_block(lines: &[String]) -> (Vec<Stmt>, Term, usize) {
    let (mut stmts, mut term, mut unsup) = (vec![], None::<Term>, 0);
    for ln in lines {
        let s = ln.trim();
        if s.is_empty() {
            continue;
        }
        if s.starts_with("drop(") && s.contains("-> [") {
            if let Some(arrow) = s.find("-> [") {
                let place = s["drop(".len()..].split(')').next().unwrap_or("").trim();
                let meta = &s[arrow + "-> [".len()..];
                let tgt = find_id(meta, "return").unwrap_or_else(|| "bb_unknown".to_string());
                term = Some(Term::Drop { place: base_local(place).unwrap_or_else(|| place.to_string()), target: tgt });
                continue;
            }
        }
        if s.starts_with("switchInt(") && s.contains("-> [") {
            if let Some(arrow) = s.rfind(") -> [") {
                let discr = parse_operand(s["switchInt(".len()..arrow].trim());
                let meta = &s[arrow + ") -> [".len()..];
                let mut targets = vec![];
                for kv in meta.split(',') {
                    let kv = kv.trim().trim_end_matches(|c| c == ']' || c == ';');
                    if kv.starts_with("otherwise:") {
                        continue;
                    }
                    if let Some(ci) = kv.find(':') {
                        let (v, t) = (kv[..ci].trim(), kv[ci + 1..].trim());
                        if t.starts_with("bb") {
                            targets.push((v.to_string(), t.chars().take_while(|c| c.is_alphanumeric()).collect()));
                        }
                    }
                }
                let ow = find_id(meta, "otherwise").unwrap_or_else(|| "bb_unknown".to_string());
                term = Some(Term::SwitchInt { discr: Box::new(discr), targets, otherwise: ow });
                continue;
            }
        }
        if s.starts_with("assert(") && s.contains("-> [") {
            if let Some(arrow) = s.rfind(") -> [") {
                let inner = &s["assert(".len()..arrow];
                let first = split_top(inner).into_iter().next().unwrap_or_default();
                let meta = &s[arrow + ") -> [".len()..];
                let tgt = find_id(meta, "success").unwrap_or_else(|| "bb_unknown".to_string());
                term = Some(Term::Assert { cond: Box::new(parse_operand(&first)), target: tgt, msg: Some(inner.chars().take(80).collect()) });
                continue;
            }
        }
        if s.starts_with("goto") {
            if let Some(t) = s.split("->").nth(1) {
                let t: String = t.trim().trim_end_matches(';').chars().take_while(|c| c.is_alphanumeric()).collect();
                term = Some(Term::Goto { target: t });
                continue;
            }
        }
        if s == "return;" || s == "return" {
            term = Some(Term::Return);
            continue;
        }
        if s == "resume;" || s == "resume" {
            term = Some(Term::Resume);
            continue;
        }
        if s.contains("-> [") {
            if let Some((dest, func, raw, args, target)) = parse_call(s) {
                match dest {
                    Some(d) => {
                        term = Some(Term::Call {
                            func,
                            func_raw: Some(raw.chars().take(100).collect()),
                            args,
                            dest: Some(d),
                            target,
                        });
                    }
                    None => {
                        stmts.push(Stmt::Assign {
                            dest: "_0".to_string(),
                            dest_place: Some("_0".to_string()),
                            rvalue: Rvalue::Call { func, args },
                        });
                        term = Some(Term::Unreachable);
                    }
                }
                continue;
            }
        }
        if let Some(eq) = s.find(" = ") {
            let (d, r) = (s[..eq].trim(), s[eq + 3..].trim().trim_end_matches(';').trim());
            // skip `let` declarations (no leading _local dest)
            if d.starts_with("let ") {
                continue;
            }
            let rv = parse_rvalue(r);
            if matches!(rv, Rvalue::Unsupported { .. }) {
                unsup += 1;
            }
            let dest = sanitize_dest(d);
            stmts.push(Stmt::Assign { dest_place: Some(d.chars().take(80).collect()), dest, rvalue: rv });
            continue;
        }
        unsup += 1;
        stmts.push(Stmt::UnsupportedStmt { text: s.chars().take(120).collect() });
    }
    (stmts, term.unwrap_or(Term::Return), unsup)
}

fn extract_fn(text: &str, fname: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|l| {
        l.starts_with("fn ") && l["fn ".len()..].split('(').next().unwrap_or("") == fname
    })?;
    let mut end = lines.len();
    for i in start + 1..lines.len() {
        let l = lines[i];
        if l.starts_with("fn ") || (l.starts_with("alloc") && l.contains('(')) {
            end = i;
            break;
        }
    }
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some(lines[start..end].iter().map(|s| s.to_string()).collect())
}

fn is_bb_header(ln: &str) -> Option<String> {
    let t = ln.trim();
    if !t.starts_with("bb") {
        return None;
    }
    let rest = &t[2..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = rest[digits.len()..].trim();
    let after = after.strip_prefix("(cleanup)").unwrap_or(after).trim();
    if after.strip_prefix(':').map(|s| s.trim().strip_prefix('{').is_some()).unwrap_or(false) {
        return Some(format!("bb{}", digits));
    }
    // also accept `bb0: {` with trailing content? headers are exactly `bbN: {`
    if after == ": {" || after == ":" {
        return Some(format!("bb{}", digits));
    }
    None
}

fn stmt_kind(s: &Stmt) -> String {
    match s {
        Stmt::Assign { rvalue, .. } => format!("Assign/{}", rvalue_kind(rvalue)),
        Stmt::StorageLive { .. } => "StorageLive".to_string(),
        Stmt::StorageDead { .. } => "StorageDead".to_string(),
        Stmt::Nop { .. } => "Nop".to_string(),
        Stmt::SetDisc { .. } => "SetDisc".to_string(),
        Stmt::UnsupportedStmt { .. } => "UnsupportedStmt".to_string(),
    }
}

fn rvalue_kind(rv: &Rvalue) -> String {
    match rv {
        Rvalue::Use { .. } => "Use".to_string(),
        Rvalue::Ref { .. } => "Ref".to_string(),
        Rvalue::Call { .. } => "Call".to_string(),
        Rvalue::BinOp { op, .. } => format!("BinOp/{}", op),
        Rvalue::UnOp { op, .. } => format!("UnOp/{}", op),
        Rvalue::Cast { .. } => "Cast".to_string(),
        Rvalue::Aggregate { .. } => "Aggregate".to_string(),
        Rvalue::Discriminant { .. } => "Discriminant".to_string(),
        Rvalue::RawPtr { .. } => "RawPtr".to_string(),
        Rvalue::Repeat { .. } => "Repeat".to_string(),
        Rvalue::ThreadLocal { .. } => "ThreadLocal".to_string(),
        Rvalue::Unsupported { .. } => "Unsupported".to_string(),
    }
}

fn term_kind(t: &Term) -> &'static str {
    match t {
        Term::Goto { .. } => "Goto",
        Term::Return => "Return",
        Term::Resume => "Resume",
        Term::Call { .. } => "Call",
        Term::Drop { .. } => "Drop",
        Term::SwitchInt { .. } => "SwitchInt",
        Term::Assert { .. } => "Assert",
        Term::Unreachable => "Unreachable",
        Term::InlineAsm { .. } => "InlineAsm",
        Term::Unsupported { .. } => "T/Unsupported",
    }
}

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
    for f in &mir.functions {
        let mut kinds = std::collections::BTreeMap::<String, usize>::new();
        let mut unsup = vec![];
        for b in &f.blocks {
            for s in &b.statements {
                tot_stmts += 1;
                *kinds.entry(stmt_kind(s)).or_insert(0) += 1;
                match s {
                    Stmt::Assign { dest, dest_place, rvalue } => {
                        // Mirror lower(): only count what lower() cannot emit.
                        let fails = match rvalue {
                            Rvalue::Unsupported { .. } => true,
                            // ThreadLocal lowers to sa_thread_local_slot (sci registry).
                            Rvalue::Repeat { op, len } => match op.as_ref() {
                                Operand::Const { value } => repeat_plan(value, len).is_none(),
                                _ => true,
                            },
                            Rvalue::Aggregate { elems } if elems.len() > 1 => match dest_place {
                                Some(p) => {
                                    lower_array_init(&b.id, p, elems).is_none()
                                        && lower_adt_init(&b.id, dest, p, elems).is_none()
                                }
                                None => true,
                            },
                            _ => false,
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
            if let Term::InlineAsm { text } = &b.terminator {
                tot_unsup += 1;
                unsup.push(format!("{}: T/InlineAsm({})", b.id, text.chars().take(60).collect::<String>()));
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
    let mir = MirFile { source: Some("rustc -Zunpretty=mir (real compiler output)".to_string()), functions: vec![Function { name: fname, locals: vec![], blocks }] };
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

    fn c(v: &str) -> Operand {
        Operand::Const { value: v.to_string() }
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
        let lines = lower_adt_init("bb10", "_19", "_19", &elems).expect("range must lower");
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
        let lines = lower_adt_init("bb13", "_37", "_37", &elems).expect("mixed tuple must lower");
        assert_eq!(lines[1], "_agg_bb13 = alloc 17");
        assert_eq!(lines[2], "store _agg_bb13+0, 1 as i32");
        assert_eq!(lines[3], "store _agg_bb13+8, ^_38 as u64");
        assert_eq!(lines[4], "store _agg_bb13+16, 1 as u8");
        assert_eq!(lines[5], "_37 = _agg_bb13");
    }

    #[test]
    fn adt_generic_two_moves() {        // f_generic bb2: (Move _2, Move _4) -> two pointer slots, moves visible.
        let elems = vec![
            Operand::Move { place: "_2".to_string() },
            Operand::Move { place: "_4".to_string() },
        ];
        let lines = lower_adt_init("bb2", "_0", "_0", &elems).expect("generic tuple must lower");
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
        let line = render_rvalue(&rv, "_3", Some("_3"), &mut unsup, "bb0");
        assert!(unsup.is_empty());
        assert_eq!(line, format!("_3 = call @sa_thread_local_slot({}) // thread-local: {}", tls_key(def), def));
    }
}
