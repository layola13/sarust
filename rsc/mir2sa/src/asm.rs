//! mir2sa::asm.
use crate::mir::Operand;
use crate::render_util::{const_needs_loud, render_operand};

/// Exact-gate for a terminator-level `asm!` that is provably a pure
/// register copy: template `mov {0}, {1}` (no operand modifiers), empty
/// options (no noreturn/nomem-style flags that change control or memory
/// semantics), exactly one plain-local out and one Copy/Move/Const in.
/// x86 `mov` affects no flags, so `out = in` is exact; the MIR operand kind
/// (Copy vs Move) is preserved via render_operand, keeping `^` visible.
/// Anything else returns None (caller stays loud UNSUPPORTED, counted).
pub fn asm_mov_copy(
    template: Option<&str>,
    options: Option<&str>,
    modifiers: bool,
    inout: bool,
    outs: &[String],
    ins: &[Operand],
) -> Option<(String, String)> {
    if inout {
        return None;
    }
    let t = template?.trim().to_lowercase();
    if t != "mov {0}, {1}" {
        return None;
    }
    if modifiers {
        return None;
    }
    if options.map(|o| !o.trim().is_empty()).unwrap_or(false) {
        return None;
    }
    if outs.len() != 1 || ins.len() != 1 {
        return None;
    }
    // Raw const values carry ForbiddenSyntax text; only clean consts pass.
    if const_needs_loud(&ins[0]) {
        return None;
    }
    let dest = outs[0].trim();
    if !is_plain_local(dest) {
        return None;
    }
    if !matches!(ins[0], Operand::Copy { .. } | Operand::Move { .. } | Operand::Const { .. }) {
        return None;
    }
    Some((dest.to_string(), render_operand(&ins[0])))
}

/// Width in bits for a short scalar ty name (`u32`->32, `usize`->64, …).
/// Pointer/reference markers (`ptr`,`&`,`*`) are 64. None if unknown.
pub fn scalar_width_bits(ty: &str) -> Option<u32> {
    match ty.trim() {
        "bool" | "u8" | "i8" => Some(8),
        "u16" | "i16" => Some(16),
        "u32" | "i32" | "f32" => Some(32),
        "u64" | "i64" | "usize" | "isize" | "f64" | "ptr" | "&" | "*" => Some(64),
        "u128" | "i128" => Some(128),
        _ => None,
    }
}

pub fn is_signed_ty(ty: &str) -> bool {
    matches!(ty.trim(), "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "f32" | "f64")
}

/// Cast lowering decision: plain copy (bit-identical) or convert via
/// a named SA instruction (emitted as `dest = mnem src as TY`).
pub enum CastLower {
    Copy,
    Convert(&'static str),
}

/// Destination short ty name from driver Debug text: scalar names pass
/// through, pointer spellings collapse to `ptr`, else unrecognized.
pub fn cast_dst_short(ty: &str) -> String {
    let t = ty.trim();
    if scalar_width_bits(t).is_some() {
        t.to_string()
    } else if t.starts_with('*') || t.starts_with('&') {
        "ptr".to_string()
    } else {
        "?".to_string()
    }
}

/// Exact SA lowering for a MIR cast given (kind, src_ty, dst_ty short names).
/// Same-width bit-identical casts (sign changes, ptr<->ptr/int, unbox) are
/// plain copies; int width changes pick sext/zext/trunc by signedness;
/// float/int crossings pick fptosi/sitofp/uitofp. Unsized/fn-ptr coercions
/// and anything unrecognized return None (caller stays loud, counted).
pub fn lower_cast(kind: &str, src_ty: &str, dst_ty: &str) -> Option<CastLower> {
    use CastLower::{Convert, Copy};
    let k = kind.trim();
    // Pointer identities and same-width reinterprets: value preserved.
    if k.starts_with("PtrToPtr")
        || k.starts_with("Transmute")
        || k.starts_with("BoxDerefTransmute")
        || k.starts_with("PointerExposeAddress")
        || k.starts_with("PointerFromExposedAddress")
    {
        return Some(Copy);
    }
    if k.starts_with("PointerCoercion") || k.starts_with("FnPtr") {
        return None;
    }
    if k.starts_with("IntToInt") {
        let (sw, dw) = (scalar_width_bits(src_ty)?, scalar_width_bits(dst_ty)?);
        if sw == dw {
            return Some(Copy);
        }
        if dw > sw {
            return Some(Convert(if is_signed_ty(src_ty) { "sext" } else { "zext" }));
        }
        return Some(Convert("trunc"));
    }
    if k.starts_with("FloatToInt") {
        scalar_width_bits(dst_ty)?;
        return Some(Convert("fptosi"));
    }
    if k.starts_with("IntToFloat") {
        scalar_width_bits(src_ty)?;
        return Some(Convert(if is_signed_ty(src_ty) { "sitofp" } else { "uitofp" }));
    }
    if k.starts_with("FloatToFloat") {
        let (sw, dw) = (scalar_width_bits(src_ty)?, scalar_width_bits(dst_ty)?);
        if sw == dw {
            return Some(Copy);
        }
        return Some(Convert(if dw > sw { "fpext" } else { "fptrunc" }));
    }
    None
}

/// Plain `_N` local (matches mir2sa parse's base_local contract).
pub fn is_plain_local(s: &str) -> bool {
    let s = s.strip_prefix('_').unwrap_or("");
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// Strip C block comments (`/* … */`, non-nesting) from an asm template;
/// unclosed comment -> None (caller stays loud). Placeholders survive
/// stripping (they render into comments, never into emitted code).
pub fn strip_asm_comments(template: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = template;
    loop {
        match rest.find("/*") {
            None => {
                out.push_str(rest);
                break;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                let after = &rest[i + 2..];
                match after.find("*/") {
                    None => return None,
                    Some(j) => rest = &after[j + 2..],
                }
            }
        }
    }
    Some(out)
}

/// Exact-gate for a value-stable `inout` escape (sla-117 shape): comment-only
/// template (nothing emitted), empty options, single plain-local out, single
/// Copy/Move/Const in. With no emitted code the register still holds the
/// input, so the output equals the input: same-place outs emit only a
/// passthrough comment, split places emit `out = in` (plain `=` moves).
/// Anything else -> None (loud, counted).
pub fn asm_inout_passthrough(
    template: Option<&str>,
    options: Option<&str>,
    modifiers: bool,
    inout: bool,
    outs: &[String],
    ins: &[Operand],
) -> Option<(Option<String>, Option<String>)> {
    if !inout || modifiers {
        return None;
    }
    let stripped = strip_asm_comments(template?)?;
    if !stripped.trim().is_empty() {
        return None;
    }
    if options.map(|o| !o.trim().is_empty()).unwrap_or(false) {
        return None;
    }
    if outs.len() != 1 || ins.len() != 1 {
        return None;
    }
    // Raw const values carry ForbiddenSyntax text; only clean consts pass.
    if const_needs_loud(&ins[0]) {
        return None;
    }
    let dest = outs[0].trim();
    if !is_plain_local(dest) {
        return None;
    }
    if matches!(&ins[0], Operand::Copy { place } | Operand::Move { place } if place == dest) {
        // Same local in and out: value already home, comment only.
        return Some((None, None));
    }
    // Split places (or an immediate): materialize the passthrough copy.
    Some((Some(dest.to_string()), Some(render_operand(&ins[0]))))
}
