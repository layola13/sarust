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
    // Field-precision: a projected input renders as a field load, not
    // a reg copy, so it fails the gate and stays loud instead of
    // copying the root local.
    if crate::render_util::operand_is_projected(&ins[0])
        || crate::render_util::operand_proj_loud(&ins[0])
    {
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
/// NonNull<T> is a repr(transparent) single-pointer wrapper, so it is
/// pointer-valued too: the driver Debug spelling std::ptr::NonNull<...>
/// (possibly truncated at 60 chars, so the generic tail may be missing)
/// collapses to ptr like raw/reference spellings do. Without this, a cast
/// to NonNull returned ? and cast_spill_ty returned None, so the value
/// could never get a spill slot.
pub fn cast_dst_short(ty: &str) -> String {
    let t = ty.trim();
    if scalar_width_bits(t).is_some() {
        t.to_string()
    } else if t.starts_with('*')
        || t.starts_with('&')
        || t.starts_with("std::ptr::NonNull")
        || t.starts_with("core::ptr::NonNull")
    {
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

/// Base local of a (possibly versioned) register: `_4_v1` -> `_4`,
/// `_1` -> `_1`. Anything else passes through unchanged (mirrors
/// spill::strip_version, which is private to that module).
pub fn strip_version_suffix(reg: &str) -> &str {
    if let Some(idx) = reg.rfind("_v") {
        let (base, suffix) = reg.split_at(idx);
        if !suffix[2..].is_empty()
            && suffix[2..].chars().all(|ch| ch.is_ascii_digit())
            && is_plain_local(base)
        {
            return base;
        }
    }
    reg
}

/// Strip one reference prefix from a driver Debug type: `&`, an optional
/// lifetime (`'{erased}`, `'^0`, ...), and an optional `mut`. Returns the
/// pointee text, or None when the type is not a reference (raw pointers
/// are not refs: their unsizing is out of scope for the slice-cast cluster).
fn strip_ref_prefix(ty: &str) -> Option<&str> {
    let t = ty.trim().strip_prefix('&')?.trim_start();
    let t = if let Some(rest) = t.strip_prefix('\'') {
        match rest.find('}') {
            Some(i) => rest[i + 1..].trim_start(),
            None => return None,
        }
    } else {
        t
    };
    let t = t.strip_prefix("mut").map(str::trim_start).unwrap_or(t);
    Some(t)
}

/// Length of a sized array pointee, or None: `[T; N_usize]` -> N.
/// The separator is the TOP-LEVEL `;` inside the outer brackets, so a
/// nested sized array element (`[[i32; 2_usize]; 3_usize]`) still parses
/// as 3. An already-unsized slice (`[T]`) has no separator -> None.
fn array_len_of_ref(src_ty: &str) -> Option<u64> {
    let pointee = strip_ref_prefix(src_ty)?.trim();
    let inner = pointee.strip_prefix('[')?.strip_suffix(']')?;
    let mut depth = 0usize;
    let mut sep = None;
    for (i, ch) in inner.char_indices() {
        match ch {
            '[' | '(' | '<' => depth += 1,
            ']' | ')' | '>' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                sep = Some(i);
                break;
            }
            _ => {}
        }
    }
    let len_text = inner[sep? + 1..].trim();
    let digits: String = len_text.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    if len_text != digits && len_text != format!("{}_usize", digits) {
        return None;
    }
    digits.parse().ok()
}

/// True when the type is an unsized slice reference (`&[T]`, `&mut [T]`,
/// `&[&str]`): a reference whose pointee is brackets with NO top-level
/// `;`. Trait objects (`dyn`) are excluded: they need a vtable pointer,
/// not a length, and stay loud (Phase 3 territory).
fn is_unsized_slice_ref(dst_ty: &str) -> bool {
    if dst_ty.contains("dyn") {
        return false;
    }
    let Some(pointee) = strip_ref_prefix(dst_ty) else {
        return false;
    };
    let pointee = pointee.trim();
    let Some(inner) = pointee.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return false;
    };
    let mut depth = 0usize;
    for ch in inner.chars() {
        match ch {
            '[' | '(' | '<' => depth += 1,
            ']' | ')' | '>' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => return false,
            _ => {}
        }
    }
    true
}

/// Slice-unsizing length for a MIR cast, or None.
/// Gates (all must hold; anything else falls through to the existing
/// lower_cast / loud path unchanged):
/// - kind is `PointerCoercion(Unsize, ...)` (reify/closure fn-pointer
///   coercions, which share the PointerCoercion prefix, are excluded);
/// - the SOURCE local's driver type (local_types, not the truncated
///   `src_ty` field, which is just `"&"`) is a sized array reference;
/// - the destination is an unsized slice reference.
/// The returned length is the array length N: the fat pointer for
/// `&[T; N] -> &[T]` is the (data_ptr, N) pair, data_ptr unchanged.
pub fn unsize_slice_len(kind: &str, src_local_ty: &str, dst_ty: &str) -> Option<u64> {
    let k = kind.trim();
    if !k.starts_with("PointerCoercion") || !k.contains("Unsize") {
        return None;
    }
    if !is_unsized_slice_ref(dst_ty) {
        return None;
    }
    array_len_of_ref(src_local_ty)
}

/// Plain `_N` local or a versioned `_N_vK` rename (see version.rs).
/// Matches mir2sa parse's base_local contract on unversioned names.
pub fn is_plain_local(s: &str) -> bool {
    let s = s.strip_prefix('_').unwrap_or("");
    if s.is_empty() {
        return false;
    }
    // Strip an optional `_vK` version suffix first.
    let core = match s.rfind("_v") {
        Some(i) if s[i + 2..].chars().all(|c| c.is_ascii_digit()) && !s[i + 2..].is_empty() => &s[..i],
        _ => s,
    };
    !core.is_empty() && core.chars().all(|c| c.is_ascii_digit())
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
    if matches!(&ins[0], Operand::Copy { place, off, deref, proj_loud, pty, .. } | Operand::Move { place, off, deref, proj_loud, pty, .. } if place == dest && *off == 0 && !*deref && !*proj_loud && pty.as_deref() != Some("buf")) {
        // Same local in and out: value already home, comment only.
        return Some((None, None));
    }
    // Field-precision: a projected input cannot passthrough as a
    // root copy (same reason as asm_mov_copy); it stays loud.
    if crate::render_util::operand_is_projected(&ins[0])
        || crate::render_util::operand_proj_loud(&ins[0])
    {
        return None;
    }
    // Split places (or an immediate): materialize the passthrough copy.
    Some((Some(dest.to_string()), Some(render_operand(&ins[0]))))
}

#[cfg(test)]
mod asm_unsize_tests {
    use super::*;

    #[test]
    fn unsize_slice_len_gates() {
        let k = "PointerCoercion(Unsize, Implicit)";
        assert_eq!(
            unsize_slice_len(k, "&'{erased} [i32; 4_usize]", "&'{erased} [i32]"),
            Some(4)
        );
        assert_eq!(
            unsize_slice_len(k, "&'{erased} mut [u8; 10_usize]", "&'{erased} mut [u8]"),
            Some(10)
        );
        assert_eq!(
            unsize_slice_len(
                k,
                "&'{erased} [&'{erased} str; 3_usize]",
                "&'{erased} [&'{erased} str]"
            ),
            Some(3)
        );
        // Nested sized array element: outer length wins.
        assert_eq!(
            unsize_slice_len(k, "&'{erased} [[i32; 2_usize]; 3_usize]", "&'{erased} [[i32; 2_usize]]"),
            Some(3)
        );
        // Dyn target: vtable, not a length.
        assert_eq!(
            unsize_slice_len(k, "&'{erased} Button", "&'{erased} dyn [Binder { value: Trait(Draw), bound_vars: [] }]"),
            None
        );
        // Fn-pointer coercions share the prefix but are not Unsize.
        assert_eq!(
            unsize_slice_len("PointerCoercion(ReifyFnPointer(Safe), Implicit)", "", "fn(i32) -> i32"),
            None
        );
        // Already-unsized source has no length to read.
        assert_eq!(
            unsize_slice_len(k, "&'{erased} [i32]", "&'{erased} [i32]"),
            None
        );
        // Sized destination is not an unsizing.
        assert_eq!(
            unsize_slice_len(k, "&'{erased} [i32; 4_usize]", "&'{erased} [i32; 4_usize]"),
            None
        );
    }
}
