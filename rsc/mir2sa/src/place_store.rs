//! mir2sa::place_store — C5 rebind repair.
//!
//! Diagnosis (2bc7d86 sweep): all 11 `UNSUPPORTED rebind` markers are NOT
//! same-path redefinitions of a local. Each is a MIR *place store*: the
//! statement writes through a place (`(*_1)`, `((*_1).0: i32)`, `(_1.1:
//! bool)`, `(*_1)[_8]`) whose base local the driver reports as `dest`.
//! lower() only looked at `dest`, saw it already bound, and dropped the
//! store (the new value never won). The repair lowers the store itself —
//! `store base+off, value as ty` — and binds nothing, so the base
//! register, its spill slot and any phi slot are untouched (a place store
//! mutates the pointee, never the pointer).
//!
//! Gates (anything outside stays on the old loud path):
//! - rvalue is a plain Use (Move/Copy/Const); no phi markers anywhere;
//! - the place parses as deref / deref-field / direct-field / deref-index
//!   over a plain local (no `as` variant projections, no nested places);
//! - scalar value types only, plus the `&str` fat-pointer double store;
//! - field offsets come from a crate-wide rustc layout harvested from
//!   Aggregate inits of the same type name, or from field 0 of a
//!   provably single-field type; conflicting layouts for one type name
//!   void the index entry (stay loud, never guess);
//! - the base is provably a buffer pointer in SA (reference-typed local,
//!   ptr param, multi-elem Aggregate def, or thin raw-pointer base);
//!   phi-managed bases stay loud (the slot, not the register, owns the
//!   current pointer value); raw-pointer bases defined by an
//!   unresolvable const (a static address rendered as placeholder 0)
//!   stay loud too (118, Jev scope_118 = wait_static, jev-1.14.0);
//! - a raw-pointer-typed cell stores as `ptr` (182: `(_1.1: *const u8)`
//!   field store); a const raw-pointer value stays loud (no trustworthy
//!   address);
//! - the value const (if any) is not a loud const, and a fat value never
//!   comes from a const-propagated placeholder (loading from address 0
//!   would be worse than loud).
//!
//! Coverage mirrors lower() through `plan_place_store` with an empty
//! constmap: a fat value fed by a loud-placeholder local would need a
//! `&str` field store whose value local was itself a loud dest, which does
//! not occur in the corpus; the sweep gate reads lower()'s count anyway.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::asm::{is_plain_local, strip_version_suffix};
use crate::const_util::{array_elem_size, sa_scalar_ty};
use crate::mir::{Function, Operand, Rvalue, Stmt};
use crate::render_util::{assign_loud_const, const_scalar_text, field_value_temp, operand_is_projected, operand_proj_loud};
use crate::spill::{spill_slot, SpillMap};

/// Crate-wide ADT layout facts harvested from Aggregate inits.
#[derive(Default, Clone)]
pub struct AdtLayoutIndex {
    /// Type name -> rustc field offsets (only when exactly one layout
    /// was seen for that name).
    pub layouts: BTreeMap<String, Vec<u64>>,
    /// Type name -> field count (only when exactly one count was seen).
    pub field_counts: BTreeMap<String, usize>,
}

fn type_of_local(local_types: &BTreeMap<String, String>, local: &str) -> Option<String> {
    if let Some(t) = local_types.get(local) {
        return Some(t.clone());
    }
    let base = strip_version_suffix(local);
    local_types.get(base).cloned()
}

pub fn build_adt_layout_index(functions: &[Function]) -> AdtLayoutIndex {
    let mut idx = AdtLayoutIndex::default();
    let mut bad_layout: BTreeSet<String> = BTreeSet::new();
    let mut bad_count: BTreeSet<String> = BTreeSet::new();
    for f in functions {
        for b in &f.blocks {
            for st in &b.statements {
                if let Stmt::Assign {
                    dest,
                    rvalue: Rvalue::Aggregate { elems, layout },
                    ..
                } = st
                {
                    let Some(ty) = type_of_local(&f.local_types, dest) else {
                        continue;
                    };
                    let ty = ty.trim().to_string();
                    if ty.is_empty() {
                        continue;
                    }
                    match idx.field_counts.get(&ty) {
                        None => {
                            idx.field_counts.insert(ty.clone(), elems.len());
                        }
                        Some(&n) if n != elems.len() => {
                            bad_count.insert(ty.clone());
                        }
                        _ => {}
                    }
                    if let Some(l) = layout {
                        if l.offsets.len() == elems.len() && !elems.is_empty() {
                            match idx.layouts.get(&ty) {
                                None => {
                                    idx.layouts.insert(ty.clone(), l.offsets.clone());
                                }
                                Some(o) if *o != l.offsets => {
                                    bad_layout.insert(ty.clone());
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }
    for t in bad_layout {
        idx.layouts.remove(&t);
    }
    for t in bad_count {
        idx.field_counts.remove(&t);
    }
    idx
}

/// A place store lower() can emit exactly, or None (caller stays loud).
pub struct PlaceStorePlan {
    /// SA register holding the buffer pointer.
    base: String,
    /// Literal byte offset for field/deref stores; None for index stores.
    offset: Option<u64>,
    /// Index register for `base[idx]` stores.
    index_reg: Option<String>,
    /// Element byte size for index stores.
    elem_size: usize,
    /// SA store type for scalar stores.
    sa_ty: String,
    /// `&str` field: emit a (ptr, len) double store instead.
    fat: bool,
    /// Rendered const value text, when the rvalue is a Const.
    value_text: Option<String>,
    /// Move/Copy source local, when the rvalue is not a Const.
    value_place: Option<String>,
    /// True when the source is a Copy (spill reload applies).
    value_is_copy: bool,
}

/// Strip one balanced outer paren layer, if the whole string is wrapped.
fn strip_outer_parens(s: &str) -> &str {
    let s = s.trim();
    if !s.starts_with('(') || !s.ends_with(')') {
        return s;
    }
    let mut depth = 0i32;
    for (i, ch) in s.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i + 1 != s.len() {
                    return s;
                }
            }
            _ => {}
        }
    }
    if depth == 0 {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Split `place: Type` at the last depth-0 `": "`; returns (place, type).
fn split_place_type(s: &str) -> (&str, Option<String>) {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut last: Option<usize> = None;
    let mut i = 0usize;
    while i + 1 < b.len() {
        match b[i] {
            b'(' | b'[' | b'<' => depth += 1,
            b')' | b']' | b'>' => depth -= 1,
            b':' if depth == 0 && b[i + 1] == b' ' => {
                last = Some(i);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    match last {
        Some(p) => (s[..p].trim(), Some(s[p + 2..].trim().to_string())),
        None => (s.trim(), None),
    }
}

/// `&'lifetime mut T` / `&T` -> `T`; None for non-references.
fn strip_ref(ty: &str) -> Option<String> {
    let mut t = ty.trim();
    t = t.strip_prefix('&')?.trim_start();
    if let Some(rest) = t.strip_prefix('\'') {
        // Lifetime token up to the next whitespace (e.g. `'{erased}`).
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        t = rest[end..].trim_start();
    }
    if let Some(rest) = t.strip_prefix("mut ") {
        t = rest.trim_start();
    }
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// `[Elem; Len]` -> Elem; None otherwise.
fn array_elem(ty: &str) -> Option<String> {
    let t = ty.trim();
    let inner = t.strip_prefix('[')?.strip_suffix(']')?;
    let semi = inner.rfind(';')?;
    let elem = inner[..semi].trim();
    if elem.is_empty() || inner[semi + 1..].trim().is_empty() {
        None
    } else {
        Some(elem.to_string())
    }
}

fn scalar_sa_ty(ty: &str) -> Option<String> {
    match ty.trim() {
        "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "f32" | "f64" | "usize"
        | "isize" | "bool" => Some(sa_scalar_ty(ty.trim()).to_string()),
        _ => None,
    }
}

/// `*const T` / `*mut T` -> `T`; None otherwise.
fn strip_raw_ptr(ty: &str) -> Option<String> {
    let t = ty.trim();
    for p in ["*const ", "*mut "] {
        if let Some(rest) = t.strip_prefix(p) {
            let inner = rest.trim();
            if !inner.is_empty() {
                return Some(inner.to_string());
            }
        }
    }
    None
}

/// True when `ty` is a thin raw pointer. Raw pointers are one word in
/// SA, so a raw-pointer-typed cell stores as `ptr`.
fn is_raw_ptr(ty: &str) -> bool {
    strip_raw_ptr(ty).is_some()
}

/// True when the defining Assign of `base` in this function (dest_place
/// equals dest, so place stores do not count) carries an unresolvable
/// const. Such a base renders as a 0 placeholder, not a real address.
fn base_is_loud_const(f: &Function, base: &str) -> bool {
    let stripped = strip_version_suffix(base);
    f.blocks.iter().flat_map(|b| &b.statements).any(|st| match st {
        Stmt::Assign {
            dest,
            dest_place,
            rvalue,
            ..
        } => {
            dest_place.as_deref() == Some(dest.as_str())
                && strip_version_suffix(dest) == stripped
                && assign_loud_const(rvalue).is_some()
        }
        _ => false,
    })
}

/// True when `ty` (a field suffix) is a `&str` fat pointer.
fn is_fat_str(ty: &str) -> bool {
    let t = ty.trim();
    if !t.starts_with('&') {
        return false;
    }
    strip_ref(t).is_some_and(|p| p == "str")
}

fn is_param_ptr(f: &Function, base: &str) -> bool {
    let s = strip_version_suffix(base);
    let Some(rest) = s.strip_prefix('_') else {
        return false;
    };
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Ok(i) = rest.parse::<usize>() else {
        return false;
    };
    i >= 1 && i <= f.params.len() && f.params[i - 1].trim() == "ptr"
}

fn has_multi_elem_agg_def(f: &Function, base: &str) -> bool {
    let stripped = strip_version_suffix(base);
    f.blocks.iter().flat_map(|b| &b.statements).any(|st| match st {
        Stmt::Assign {
            dest,
            rvalue: Rvalue::Aggregate { elems, .. },
            ..
        } => elems.len() > 1 && strip_version_suffix(dest) == stripped,
        _ => false,
    })
}

enum PlaceShape {
    /// `*base` — store the whole pointee.
    Deref { base: String },
    /// `(*base).N` or `base.N` — store one struct field.
    Field { base: String, deref: bool, field: usize },
    /// `(*base)[idx]` or `base[idx]` — store one array element.
    Index { base: String, idx: String },
}

fn parse_place(place: &str) -> Option<PlaceShape> {
    let p = place.trim();
    // Simple shapes never contain spaces; anything else (nested places
    // with type suffixes inside) is rejected here.
    if p.is_empty() || p.contains(' ') {
        return None;
    }
    // Index first: it ends with `]`.
    if let Some(rb) = p.strip_suffix(']') {
        let lb = rb.rfind('[')?;
        let head = rb[..lb].trim();
        let idx = rb[lb + 1..].trim();
        if !is_plain_local(idx) {
            return None;
        }
        let base = if let Some(inner) = head.strip_prefix("(*").and_then(|h| h.strip_suffix(')')) {
            inner.trim()
        } else {
            head
        };
        if !is_plain_local(base) {
            return None;
        }
        return Some(PlaceShape::Index {
            base: base.to_string(),
            idx: idx.to_string(),
        });
    }
    // Deref forms: `(*base)` or `(*base).N`.
    if let Some(rest) = p.strip_prefix("(*") {
        let close = rest.find(')')?;
        let base = rest[..close].trim();
        if !is_plain_local(base) {
            return None;
        }
        let tail = rest[close + 1..].trim();
        if tail.is_empty() {
            return Some(PlaceShape::Deref {
                base: base.to_string(),
            });
        }
        let field = tail.strip_prefix('.')?;
        if field.is_empty() || !field.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        return Some(PlaceShape::Field {
            base: base.to_string(),
            deref: true,
            field: field.parse().ok()?,
        });
    }
    // Pure deref: `*base`.
    if let Some(base) = p.strip_prefix('*') {
        if is_plain_local(base.trim()) {
            return Some(PlaceShape::Deref {
                base: base.trim().to_string(),
            });
        }
        return None;
    }
    // Direct field: `base.N`.
    if let Some(dot) = p.find('.') {
        let (head, tail) = (&p[..dot], &p[dot + 1..]);
        if is_plain_local(head) && !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return Some(PlaceShape::Field {
                base: head.to_string(),
                deref: false,
                field: tail.parse().ok()?,
            });
        }
    }
    None
}

/// Pure predicate shared by lower() and coverage(): Some(plan) iff this
/// Assign is an exactly-lowerable place store. Never panics on driver
/// text; anything unrecognized is None (caller stays loud).
#[allow(clippy::too_many_arguments)]
pub fn plan_place_store(
    dest_place: Option<&str>,
    dest: &str,
    rvalue: &Rvalue,
    f: &Function,
    layouts: &AdtLayoutIndex,
    phi_b: &BTreeMap<String, String>,
    cmap: &HashMap<String, String>,
) -> Option<PlaceStorePlan> {
    let full = dest_place?;
    if full.trim() == dest.trim() {
        return None;
    }
    if full.contains(" as ") {
        return None;
    }
    if assign_loud_const(rvalue).is_some() {
        return None;
    }
    let Rvalue::Use { op } = rvalue else {
        return None;
    };
    // Value side.
    let (value_text, value_place, value_is_copy) = match op {
        Operand::Const {
            value,
            str_bytes,
            ..
        } => {
            if str_bytes.is_some() {
                return None;
            }
            let text = const_scalar_text(value)?;
            if text.contains('(') {
                return None;
            }
            (Some(text), None, false)
        }
        Operand::Move { place , .. } | Operand::Copy { place , .. } => {
            // Field-precision: unrenderable projections stay loud
            // (coverage calls this same plan function, so the count
            // matches lower's rebind-loud path exactly).
            if operand_proj_loud(op) {
                return None;
            }
            if !is_plain_local(place) {
                return None;
            }
            if crate::phi::phi_place(place).is_some() {
                return None;
            }
            (None, Some(place.clone()), matches!(op, Operand::Copy { .. }))
        }
        Operand::Conflict { .. } => return None,
    };
    // Place side.
    let stripped = strip_outer_parens(full);
    let (place, suffix_ty) = split_place_type(stripped);
    let shape = parse_place(place)?;
    let (base, deref) = match &shape {
        PlaceShape::Deref { base } => (base.clone(), true),
        PlaceShape::Field { base, deref, .. } => (base.clone(), *deref),
        PlaceShape::Index { base, .. } => (base.clone(), true),
    };
    // Phi-managed bases: the slot owns the current pointer, not the reg.
    if phi_b.contains_key(strip_version_suffix(&base)) {
        return None;
    }
    let base_ty = type_of_local(&f.local_types, &base)?;
    // Base must be a buffer pointer in SA.
    let base_is_ptr = if deref {
        // Deref forms need a reference-typed base, a ptr param, or a
        // thin raw-pointer base (W3: the 118 (*_1) shape).
        strip_ref(&base_ty).is_some() || is_param_ptr(f, &base) || is_raw_ptr(&base_ty)
    } else {
        is_param_ptr(f, &base)
            || base_ty.trim().starts_with('&')
            || has_multi_elem_agg_def(f, &base)
    };
    if !base_is_ptr {
        return None;
    }
    // Placeholder guard (Jev scope_118 = wait_static, jev-1.14.0): a
    // raw-pointer base defined by an unresolvable const (e.g. a static
    // address rendered as placeholder 0) is not a real address; storing
    // through it would write address 0. Stay loud. Reference/param
    // bases are untouched by this guard.
    if deref && is_raw_ptr(&base_ty) && base_is_loud_const(f, &base) {
        return None;
    }
    match shape {
        PlaceShape::Deref { .. } => {
            // Value type = pointee type (suffix wins when present).
            let pointee = match &suffix_ty {
                Some(t) => t.clone(),
                None => strip_ref(&base_ty).or_else(|| strip_raw_ptr(&base_ty))?,
            };
            if is_fat_str(&pointee) {
                return None;
            }
            // A raw-pointer pointee stores as `ptr`; a const
            // raw-pointer value has no trustworthy address, so it
            // stays loud.
            let sa_ty = if is_raw_ptr(&pointee) {
                if value_place.is_none() {
                    return None;
                }
                "ptr".to_string()
            } else {
                scalar_sa_ty(&pointee)?
            };
            Some(PlaceStorePlan {
                base,
                offset: Some(0),
                index_reg: None,
                elem_size: 0,
                sa_ty,
                fat: false,
                value_text,
                value_place,
                value_is_copy,
            })
        }
        PlaceShape::Field { field, .. } => {
            let suffix = suffix_ty?;
            let fat = is_fat_str(&suffix);
            // A fat value from a field cell has no single pair address
            // to load from (field temps hold one word); stay loud.
            if fat && operand_is_projected(op) {
                return None;
            }
            if fat {
                // A fat value must be a real pair address, never a
                // const-propagated placeholder literal.
                if let Some(p) = &value_place {
                    if cmap.contains_key(p) {
                        return None;
                    }
                } else {
                    return None;
                }
            }
            let sa_ty = if fat {
                "ptr".to_string()
            } else if is_raw_ptr(&suffix) {
                // W3 (182): a raw-pointer struct field stores as `ptr`.
                // A const raw-pointer value has no trustworthy address.
                if value_place.is_none() {
                    return None;
                }
                "ptr".to_string()
            } else {
                scalar_sa_ty(&suffix)?
            };
            // Offset: rustc layout for the struct type, else field 0 of
            // a provably single-field type.
            let struct_ty = if deref {
                strip_ref(&base_ty)?
            } else {
                base_ty.trim().to_string()
            };
            let offset = match layouts.layouts.get(&struct_ty) {
                Some(offsets) if field < offsets.len() => offsets[field],
                _ => {
                    if field == 0 && layouts.field_counts.get(&struct_ty) == Some(&1) {
                        0
                    } else {
                        return None;
                    }
                }
            };
            Some(PlaceStorePlan {
                base,
                offset: Some(offset),
                index_reg: None,
                elem_size: 0,
                sa_ty,
                fat,
                value_text,
                value_place,
                value_is_copy,
            })
        }
        PlaceShape::Index { idx, .. } => {
            let pointee = strip_ref(&base_ty)?;
            let elem = array_elem(&pointee)?;
            let size = array_elem_size(&elem)?;
            let sa_ty = scalar_sa_ty(&elem)?;
            Some(PlaceStorePlan {
                base,
                offset: None,
                index_reg: Some(idx),
                elem_size: size,
                sa_ty,
                fat: false,
                value_text,
                value_place,
                value_is_copy,
            })
        }
    }
}

/// Emit SA lines for a planned place store (unindented; the caller adds
/// the 4-space indent). Returns None if emission is impossible, in which
/// case the caller must fall back to the old loud path.
#[allow(clippy::too_many_arguments)]
pub fn emit_place_store(
    dest_place: Option<&str>,
    dest: &str,
    rvalue: &Rvalue,
    f: &Function,
    layouts: &AdtLayoutIndex,
    phi_b: &BTreeMap<String, String>,
    spill: &SpillMap,
    cmap: &HashMap<String, String>,
    bid: &str,
    mv_idx: &mut usize,
) -> Option<Vec<String>> {
    let plan = plan_place_store(dest_place, dest, rvalue, f, layouts, phi_b, cmap)?;
    let mut lines = vec![format!(
        "// place store {}",
        dest_place.unwrap_or("").trim()
    )];
    // Resolve the value source. A spilled Copy reloads from its slot
    // (never moves the register); a const-propagated literal
    // re-materializes; anything else reads the register directly
    // (stores never consume, so drop glue still frees it at exits).
    // Field-precision: a projected value loads its field cell into
    // a temp first (plan already rejected proj_loud and fat-projected
    // values, so this temp always holds the exact field value; loads
    // never consume the base, so no spill/cmap logic applies here).
    let src = if let Rvalue::Use { op } = rvalue {
        if operand_is_projected(op) {
            let (mut plines, tmp) = field_value_temp(op, bid, mv_idx);
            lines.append(&mut plines);
            tmp
        } else {
            match (&plan.value_text, &plan.value_place) {
                (Some(t), _) => t.clone(),
        (None, Some(p)) => {
            if plan.value_is_copy {
                if let Some(ty) = spill.get(p) {
                    let tmp = format!("_ps_{}_{}", bid, *mv_idx);
                    *mv_idx += 1;
                    lines.push(format!("{} = load {}+0 as {}", tmp, spill_slot(p), ty));
                    tmp
                } else if let Some(lit) = cmap.get(p) {
                    if plan.fat {
                        return None;
                    }
                    lit.clone()
                } else {
                    p.clone()
                }
            } else {
                p.clone()
            }
        }
                _ => return None,
            }
        }
    } else {
        return None; // unreachable: plan only accepts Use.
    };
    if plan.fat {
        let off = plan.offset?;
        let t0 = format!("_ps_{}_{}", bid, *mv_idx);
        *mv_idx += 1;
        let t1 = format!("_ps_{}_{}", bid, *mv_idx);
        *mv_idx += 1;
        // Pair layout (slice.sal): ptr at +0, len at +8.
        lines.push(format!("{} = load {}+0 as ptr", t0, src));
        lines.push(format!("{} = load {}+8 as u64", t1, src));
        lines.push(format!("store {}+{}, {} as ptr", plan.base, off, t0));
        lines.push(format!("store {}+{}, {} as u64", plan.base, off + 8, t1));
    } else if let Some(idx) = &plan.index_reg {
        // SA store offsets must be literals (probe: `store p+_8` is
        // ForbiddenSyntax), so compute the byte address with `add`
        // (probe: `add` on a ptr register checks clean).
        let ptr = format!("_ps_{}_{}", bid, *mv_idx);
        *mv_idx += 1;
        if plan.elem_size == 1 {
            lines.push(format!("{} = add {}, {}", ptr, plan.base, idx));
        } else {
            let off = format!("_ps_{}_{}", bid, *mv_idx);
            *mv_idx += 1;
            lines.push(format!("{} = mul {}, {}", off, idx, plan.elem_size));
            lines.push(format!("{} = add {}, {}", ptr, plan.base, off));
        }
        lines.push(format!("store {}+0, {} as {}", ptr, src, plan.sa_ty));
    } else {
        let off = plan.offset?;
        lines.push(format!(
            "store {}+{}, {} as {}",
            plan.base, off, src, plan.sa_ty
        ));
    }
    Some(lines)
}

#[cfg(test)]
mod place_store_tests {
    use super::*;
    use crate::mir::{AdtLayout, Block, Term};

    fn fn_with_types(
        name: &str,
        params: Vec<String>,
        local_types: Vec<(&str, &str)>,
        blocks: Vec<Block>,
    ) -> Function {
        Function {
            name: name.to_string(),
            path: None,
            locals: vec![],
            local_types: local_types
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            params,
            ret: None,
            sig_ok: true,
            blocks,
        }
    }

    fn agg_fn() -> Function {
        // Builder::new: Self { method, path } with rustc layout [0, 16].
        fn_with_types(
            "new",
            vec![],
            vec![("_0", "Builder")],
            vec![Block {
                id: "bb0".to_string(),
                statements: vec![Stmt::Assign {
                    dest: "_0".to_string(),
                    dest_place: Some("_0".to_string()),
                    rvalue: Rvalue::Aggregate {
                        elems: vec![
                            Operand::Const {
                                value: "Val(Scalar(0x00), u8)".to_string(),
                                str_bytes: None,
                                str_len: None,
                            },
                            Operand::Const {
                                value: "Val(Scalar(0x00), u8)".to_string(),
                                str_bytes: None,
                                str_len: None,
                            },
                        ],
                        layout: Some(AdtLayout {
                            size: 32,
                            offsets: vec![0, 16],
                        }),
                    },
                }],
                terminator: Term::Return { ret: None },
            }],
        )
    }

    fn use_rv(op: Operand) -> Rvalue {
        Rvalue::Use { op }
    }

    #[test]
    fn deref_scalar_store_plans() {
        let f = fn_with_types(
            "bump",
            vec!["ptr".to_string()],
            vec![("_1", "&'{erased} mut i32"), ("_2", "(i32, bool)")],
            vec![],
        );
        let idx = AdtLayoutIndex::default();
        let plan = plan_place_store(
            Some("(*_1)"),
            "_1",
            &use_rv(Operand::Move {
                place: "_2".to_string(),
            off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
            &f,
            &idx,
            &BTreeMap::new(),
            &HashMap::new(),
        )
        .expect("deref store must plan");
        assert_eq!(plan.base, "_1");
        assert_eq!(plan.offset, Some(0));
        assert_eq!(plan.sa_ty, "i32");
        assert!(!plan.fat);
    }

    #[test]
    fn field_store_uses_crate_layout() {
        let idx = build_adt_layout_index(&[agg_fn()]);
        assert_eq!(idx.layouts.get("Builder"), Some(&vec![0, 16]));
        let f = fn_with_types(
            "path",
            vec!["ptr".to_string(), "ptr".to_string()],
            vec![("_1", "Builder"), ("_2", "&'{erased} str")],
            vec![],
        );
        let plan = plan_place_store(
            Some("(_1.1: &str)"),
            "_1",
            &use_rv(Operand::Copy {
                place: "_2".to_string(),
            off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
            &f,
            &idx,
            &BTreeMap::new(),
            &HashMap::new(),
        )
        .expect("fat field store must plan");
        assert!(plan.fat);
        assert_eq!(plan.offset, Some(16));
    }

    #[test]
    fn field_zero_single_field_needs_no_layout() {
        let idx = AdtLayoutIndex {
            layouts: BTreeMap::new(),
            field_counts: [("Account".to_string(), 1)].into_iter().collect(),
        };
        let f = fn_with_types(
            "deposit",
            vec!["ptr".to_string(), "i32".to_string()],
            vec![("_1", "&'{erased} mut Account"), ("_3", "i32")],
            vec![],
        );
        let plan = plan_place_store(
            Some("((*_1).0: i32)"),
            "_1",
            &use_rv(Operand::Move {
                place: "_3".to_string(),
            off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
            &f,
            &idx,
            &BTreeMap::new(),
            &HashMap::new(),
        )
        .expect("single-field field-0 store must plan");
        assert_eq!(plan.offset, Some(0));
        assert_eq!(plan.sa_ty, "i32");
    }

    #[test]
    fn unknown_field_offset_stays_loud() {
        // Field 1 with no layout and no field count: never guess.
        let idx = AdtLayoutIndex::default();
        let f = fn_with_types(
            "f",
            vec!["ptr".to_string()],
            vec![("_1", "&'{erased} mut Foo")],
            vec![],
        );
        assert!(plan_place_store(
            Some("((*_1).1: i32)"),
            "_1",
            &use_rv(Operand::Const {
                value: "Val(Scalar(0x00000000), i32)".to_string(),
                str_bytes: None,
                str_len: None,
            }),
            &f,
            &idx,
            &BTreeMap::new(),
            &HashMap::new(),
        )
        .is_none());
    }

    #[test]
    fn index_store_plans_with_elem_size() {
        let f = fn_with_types(
            "fill_zero",
            vec!["ptr".to_string()],
            vec![("_1", "&'{erased} mut [u8; 4_usize]"), ("_8", "usize")],
            vec![],
        );
        let plan = plan_place_store(
            Some("(*_1)[_8]"),
            "_1",
            &use_rv(Operand::Const {
                value: "Val(Scalar(0x00), u8)".to_string(),
                str_bytes: None,
                str_len: None,
            }),
            &f,
            &AdtLayoutIndex::default(),
            &BTreeMap::new(),
            &HashMap::new(),
        )
        .expect("index store must plan");
        assert_eq!(plan.index_reg, Some("_8".to_string()));
        assert_eq!(plan.elem_size, 1);
        assert_eq!(plan.sa_ty, "u8");
    }

    #[test]
    fn nested_and_variant_places_stay_loud() {
        let f = fn_with_types(
            "f",
            vec!["ptr".to_string()],
            vec![("_24", "&'{erased} mut Foo")],
            vec![],
        );
        let idx = AdtLayoutIndex::default();
        let empty = HashMap::new();
        assert!(plan_place_store(
            Some("(((*_24).1: ManuallyDrop<[i32; 4]>).0: [i32; 4])"),
            "_24",
            &use_rv(Operand::Copy {
                place: "_2".to_string(),
            off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
            &f,
            &idx,
            &BTreeMap::new(),
            &empty,
        )
        .is_none());
        assert!(plan_place_store(
            Some("((_1 as Variant).0: i32)"),
            "_1",
            &use_rv(Operand::Copy {
                place: "_2".to_string(),
            off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
            &f,
            &idx,
            &BTreeMap::new(),
            &empty,
        )
        .is_none());
    }

    #[test]
    fn phi_managed_base_stays_loud() {
        let f = fn_with_types(
            "f",
            vec!["ptr".to_string()],
            vec![("_1", "&'{erased} mut i32")],
            vec![],
        );
        let phi_b: BTreeMap<String, String> =
            [("_1".to_string(), "i32".to_string())].into_iter().collect();
        assert!(plan_place_store(
            Some("(*_1)"),
            "_1",
            &use_rv(Operand::Move {
                place: "_2".to_string(),
            off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
            &f,
            &AdtLayoutIndex::default(),
            &phi_b,
            &HashMap::new(),
        )
        .is_none());
    }

    fn rawptr_agg_fn() -> Function {
        // M { fd: i32, ptr: *const u8, len: i32 } with rustc layout
        // [8, 0, 12] (field order in MIR source order, offsets reordered).
        fn_with_types(
            "new",
            vec![],
            vec![("_1", "M"), ("_2", "*const u8")],
            vec![Block {
                id: "bb0".to_string(),
                statements: vec![Stmt::Assign {
                    dest: "_1".to_string(),
                    dest_place: Some("_1".to_string()),
                    rvalue: Rvalue::Aggregate {
                        elems: vec![
                            Operand::Const {
                                value: "Val(Scalar(0x00000000), i32)".to_string(),
                                str_bytes: None,
                                str_len: None,
                            },
                            Operand::Const {
                                value: "Val(Scalar(0x00000000), *const u8)".to_string(),
                                str_bytes: None,
                                str_len: None,
                            },
                            Operand::Const {
                                value: "Val(Scalar(0x00000004), i32)".to_string(),
                                str_bytes: None,
                                str_len: None,
                            },
                        ],
                        layout: Some(AdtLayout {
                            size: 16,
                            offsets: vec![8, 0, 12],
                        }),
                    },
                }],
                terminator: Term::Return { ret: None },
            }],
        )
    }

    #[test]
    fn field_raw_ptr_store_plans() {
        // W3 (182): (_1.1: *const u8) = Copy _2 stores as ptr at the
        // rustc field offset (0 for this layout).
        let idx = build_adt_layout_index(&[rawptr_agg_fn()]);
        let f = rawptr_agg_fn();
        let plan = plan_place_store(
            Some("(_1.1: *const u8)"),
            "_1",
            &use_rv(Operand::Copy {
                place: "_2".to_string(),
                off: 0,
                off2: 0,
                deref: false,
                deref_first: false,
                pty: None,
                proj_loud: false,
                deref_last: false,
            }),
            &f,
            &idx,
            &BTreeMap::new(),
            &HashMap::new(),
        )
        .expect("raw-ptr field store must plan");
        assert_eq!(plan.base, "_1");
        assert_eq!(plan.offset, Some(0));
        assert_eq!(plan.sa_ty, "ptr");
        assert!(!plan.fat);
    }

    #[test]
    fn deref_raw_ptr_base_plans() {
        // A deref store through a genuine thin raw-pointer base plans
        // with the pointee type; the suffix is absent, so the pointee
        // comes from stripping the base type.
        let f = fn_with_types(
            "bump",
            vec![],
            vec![("_1", "*mut i32"), ("_2", "i32")],
            vec![],
        );
        let plan = plan_place_store(
            Some("(*_1)"),
            "_1",
            &use_rv(Operand::Move {
                place: "_2".to_string(),
                off: 0,
                off2: 0,
                deref: false,
                deref_first: false,
                pty: None,
                proj_loud: false,
                deref_last: false,
            }),
            &f,
            &AdtLayoutIndex::default(),
            &BTreeMap::new(),
            &HashMap::new(),
        )
        .expect("raw-ptr deref store must plan");
        assert_eq!(plan.base, "_1");
        assert_eq!(plan.offset, Some(0));
        assert_eq!(plan.sa_ty, "i32");
    }

    #[test]
    fn deref_placeholder_base_stays_loud() {
        // 118: _1 is defined by an unresolvable static-address const
        // (rendered as placeholder 0). A store through it stays loud
        // (Jev scope_118 = wait_static, jev-1.14.0), even though a
        // genuine raw-pointer base would plan.
        let f = fn_with_types(
            "main",
            vec![],
            vec![("_1", "*mut i32"), ("_2", "i32")],
            vec![Block {
                id: "bb0".to_string(),
                statements: vec![Stmt::Assign {
                    dest: "_1".to_string(),
                    dest_place: Some("_1".to_string()),
                    rvalue: use_rv(Operand::Const {
                        value: "Val(Scalar(alloc1), *mut i32)".to_string(),
                        str_bytes: None,
                        str_len: None,
                    }),
                }],
                terminator: Term::Return { ret: None },
            }],
        );
        assert!(plan_place_store(
            Some("(*_1)"),
            "_1",
            &use_rv(Operand::Move {
                place: "_2".to_string(),
                off: 0,
                off2: 0,
                deref: false,
                deref_first: false,
                pty: None,
                proj_loud: false,
                deref_last: false,
            }),
            &f,
            &AdtLayoutIndex::default(),
            &BTreeMap::new(),
            &HashMap::new(),
        )
        .is_none());
    }

    #[test]
    fn render_sa_end_to_end_builder() {
        // new() supplies the Builder layout; method() stores field 0.
        let new_f = agg_fn();
        let method_f = fn_with_types(
            "method",
            vec!["ptr".to_string(), "ptr".to_string()],
            vec![("_1", "Builder"), ("_2", "&'{erased} str")],
            vec![Block {
                id: "bb0".to_string(),
                statements: vec![
                    Stmt::Assign {
                        dest: "_1".to_string(),
                        dest_place: Some("(_1.0: &str)".to_string()),
                        rvalue: use_rv(Operand::Copy {
                            place: "_2".to_string(),
                        off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
                    },
                    Stmt::Assign {
                        dest: "_0".to_string(),
                        dest_place: Some("_0".to_string()),
                        rvalue: use_rv(Operand::Move {
                            place: "_1".to_string(),
                        off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false }),
                    },
                ],
                terminator: Term::Return {
                    ret: Some("_0".to_string()),
                },
            }],
        );
        let mir = crate::mir::MirFile {
            source: None,
            crate_name: None,
            functions: vec![new_f, method_f],
        };
        let (sa, unsup) = crate::lower::render_sa(&mir);
        assert!(
            !sa.contains("UNSUPPORTED rebind"),
            "rebind must be gone:\n{}",
            sa
        );
        assert!(
            sa.contains("store _1+0,"),
            "fat field-0 ptr store missing:\n{}",
            sa
        );
        assert!(
            sa.contains("store _1+8,"),
            "fat field-0 len store missing:\n{}",
            sa
        );
        assert!(
            !unsup.iter().any(|u| u.contains("Rebind")),
            "unsup still has rebind: {:?}",
            unsup
        );
    }
}
