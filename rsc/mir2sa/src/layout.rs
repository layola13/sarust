//! mir2sa::layout.
use crate::const_util::{array_elem_size, const_array_elem, const_elem_ty, sa_scalar_ty, scalar_hex_value};
use crate::mir::{AdtLayout, Operand, STR_INLINE_MAX};

/// `(((*_46).1: ...).0: [i32; 3])` lowers to sla-style `alloc` + `store`
/// (cf. `sci/sa_std/alloc/vec.sa`: `buf = alloc bytes`,
/// `store base+off, v as u64`). Returns SA lines or None (caller keeps it
/// UNSUPPORTED: non-const elems, unknown layout, count mismatch).
pub fn lower_array_init(bid: &str, dest_place: &str, elems: &[Operand]) -> Option<Vec<String>> {
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
            Operand::Const { value, .. } => {
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

pub fn align_agg_offset(offset: usize, size: usize) -> usize {
    // Mirrors sla alignAggregateOffset: only 8-byte fields force alignment.
    if size == 8 { (offset + 7) & !7 } else { offset }
}

/// Generic Adt (struct/tuple/range/enum-payload) lowering to sla-style
/// `alloc` + per-field `store` (cf. `sci/sa_std/alloc/vec.sa`).
/// Layout mirrors `sa_plugin_sla` tuple/struct ABI (packed except 8-byte
/// alignment). Move elements stay visible as `^p`; Const elements are
/// decimalized; unknown-typed Moves/Copies default to 8-byte `u64` slots
/// (sla `else => 8`). Zero-sized Consts (`Val(ZeroSized, …)`: PhantomData,
/// PhantomPinned) occupy 0 bytes and emit no store (exact, per Rust layout).
/// `&str` literal Consts (driver-resolved `str_bytes`, ≤64B) materialize an
/// inline byte buffer plus a (ptr,len) fat-pointer double store per slice.sal.
/// Returns SA lines bound to `dest`, or None.
pub fn lower_adt_init(
    bid: &str,
    dest: &str,
    dest_place: &str,
    elems: &[Operand],
    layout: Option<&AdtLayout>,
) -> Option<Vec<String>> {
    if elems.len() < 2 {
        return None;
    }
    let mut plans: Vec<FieldPlan> = Vec::with_capacity(elems.len());
    for e in elems {
        match e {
            Operand::Const { value, .. } if value.trim_start().starts_with("Val(ZeroSized") => {
                plans.push(FieldPlan::Store { text: None, sa_ty: "u8", size: 0 });
            }
            Operand::Const { value, str_bytes, str_len } => {
                if str_bytes.is_some() {
                    plans.push(plan_str_field(value, str_bytes.as_ref(), *str_len)?);
                    continue;
                }
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
                plans.push(FieldPlan::Store { text: Some(rendered), sa_ty: sa_scalar_ty(ty), size });
            }
            Operand::Move { place } => {
                plans.push(FieldPlan::Store { text: Some(format!("^{}", place)), sa_ty: "u64", size: 8 });
            }
            Operand::Copy { place } => {
                plans.push(FieldPlan::Store { text: Some(place.clone()), sa_ty: "u64", size: 8 });
            }
        }
    }
    // v1 footprint (also the all-zero-sized detector).
    let mut v1_total = 0usize;
    for p in &plans {
        v1_total = align_agg_offset(v1_total, p.align_size());
        v1_total += p.size();
    }
    if v1_total == 0 {
        // All fields zero-sized: no storage, bind a null marker (exact).
        return Some(vec![
            format!("// aggregate Adt init @ {} (all fields zero-sized)", dest_place.trim()),
            format!("{} = 0", dest),
        ]);
    }
    // p_layout v2: driver-supplied rustc offsets win when the arity matches.
    // They may be non-ascending (reordered fields) or non-zero-based (enum
    // payload after the tag gap) — both exact, used verbatim. Per-element
    // rendering still comes from the v1 plans above (Const decimalized via
    // its own suffix, Move/Copy as u64 slots with `^` visible).
    let (total, offsets, origin) = match layout {
        Some(l) if l.offsets.len() == elems.len() && l.size > 0 => {
            (l.size as usize, l.offsets.iter().map(|o| *o as usize).collect(), "p_layout v2: rustc layout")
        }
        _ => {
            let mut v1 = Vec::with_capacity(plans.len());
            let mut off = 0usize;
            for p in &plans {
                off = align_agg_offset(off, p.align_size());
                v1.push(off);
                off += p.size();
            }
            (off, v1, "p_layout v1: sla tuple/struct ABI")
        }
    };
    // Never under-allocate against the heuristic (e.g. Move slots assumed 8B
    // while the true field is smaller); over-allocation is harmless slack.
    let total = v1_total.max(total);
    let base = format!("_agg_{}", bid);
    let mut lines = vec![
        format!("// aggregate Adt init @ {} ({})", dest_place.trim(), origin),
        format!("{} = alloc {}", base, total),
    ];
    for (i, p) in plans.iter().enumerate() {
        match p {
            FieldPlan::Store { text: Some(v), sa_ty, .. } => {
                lines.push(format!("store {}+{}, {} as {}", base, offsets[i], v, sa_ty));
            }
            FieldPlan::Store { text: None, .. } => {}
            FieldPlan::StrLit { bytes } => {
                // Byte buffer + fat-pointer (ptr,len) per slice.sal layout.
                let buf = format!("_str_{}_{}", bid, i);
                lines.push(format!("{} = alloc {}", buf, bytes.len()));
                for (j, b) in bytes.iter().enumerate() {
                    lines.push(format!("store {}+{}, {} as u8", buf, j, b));
                }
                lines.push(format!("store {}+{}, {} as ptr", base, offsets[i], buf));
                lines.push(format!("store {}+{}, {} as u64", base, offsets[i] + 8, bytes.len()));
            }
        }
    }
    lines.push(format!("{} = {}", dest, base));
    Some(lines)
}

/// One aggregate field's lowering plan.
pub enum FieldPlan {
    /// Single `store` (text None = ZST, occupies 0B, emits nothing).
    Store { text: Option<String>, sa_ty: &'static str, size: usize },
    /// `&str` literal: byte buffer + fat-pointer (ptr,len) double store.
    /// 16B wide, 8B aligned (Slice_ptr=+0, Slice_len=+8 per slice.sal).
    StrLit { bytes: Vec<u64> },
}

impl FieldPlan {
    fn size(&self) -> usize {
        match self {
            FieldPlan::Store { size, .. } => *size,
            FieldPlan::StrLit { .. } => 16,
        }
    }
    fn align_size(&self) -> usize {
        match self {
            FieldPlan::Store { size, .. } => *size,
            FieldPlan::StrLit { .. } => 8,
        }
    }
}

/// Plan a `&str` literal field: byte buffer preamble + (ptr,len) stores.
/// Gate: driver-resolved bytes, len within STR_INLINE_MAX, counts agree.
/// Anything else -> None (caller stays loud UNSUPPORTED).
pub fn plan_str_field(value: &str, str_bytes: Option<&Vec<u64>>, str_len: Option<u64>) -> Option<FieldPlan> {
    let bytes = str_bytes?;
    let len = str_len?;
    if len > STR_INLINE_MAX || bytes.len() as u64 != len {
        return None;
    }
    // Sanity: the Debug text must name a string slice (never trust blindly).
    if !(value.contains("str") || value.contains("Slice")) {
        return None;
    }
    Some(FieldPlan::StrLit { bytes: bytes.clone() })
}

/// FNV-1a 64 of a ThreadLocal def path: the u64 key passed to
/// `sa_thread_local_slot` (see `sci/sa_std/thread_local.sai`). Keeps .sa
/// string-free; collisions across a crate's few TLS statics are impractical.
pub fn tls_key(def: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in def.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
