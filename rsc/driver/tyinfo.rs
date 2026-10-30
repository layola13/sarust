//! rsc_driver::tyinfo.
use crate::util::trunc;
use rustc_middle::mir::interpret::GlobalId;
use rustc_middle::mir::{Body, Const, ConstOperand, Operand, Place};
use rustc_middle::ty::{ConstKind, Mutability, Ty, TyCtxt, TyKind};
use rustc_middle::ty;
use rustc_span::DUMMY_SP;

/// Resolve a MIR const to a `Const::Val` when possible.
///
/// After monomorphization most consts arrive evaluated, but const items that
/// escaped substitution stay `Const::Unevaluated` — and the backend can only
/// read the Debug text of a `Val`, so those became unresolvable-const gaps.
/// `const_eval_global_id` runs the compiler's own evaluator on the item's
/// DefId; on success the backend gets the value form. Failure (generic params,
/// unsupported ops, post-mono leftovers) returns the original const untouched:
/// extraction must never die or ICE.
pub fn resolve_const<'tcx>(tcx: TyCtxt<'tcx>, c: &Const<'tcx>) -> Const<'tcx> {
    let Const::Unevaluated(uc, _) = c else {
        return *c;
    };
    let instance = ty::Instance::new_raw(uc.def, uc.args);
    let cid = GlobalId { instance, promoted: uc.promoted };
    let typing_env = ty::TypingEnv::fully_monomorphized();
    // Returns a `ConstValue` (not a `Const`): wrap it with the operand's own
    // type so the backend sees the same `Val(...)` shape it parses elsewhere.
    match tcx.const_eval_global_id(typing_env, cid, DUMMY_SP) {
        Ok(v) => {
            let ty = match c {
                Const::Unevaluated(_, t) => *t,
                _ => return *c,
            };
            Const::Val(v, ty)
        }
        Err(_) => *c,
    }
}

/// Byte-literal payload for a slice constant (`Const::Val` with
/// `ConstValue::Slice`): (bytes, len), else None.
///
/// Covers `&str` and byte containers (`&[u8; N]`, `&[u8]`, `&u8`): both are
/// laid out as a fat pointer (ptr,len) and both are materialized by the
/// backend as an inline byte buffer plus a double store, so the payload is
/// what matters, not the pointee's Rust type. UTF-8 is NOT required (byte
/// arrays are arbitrary); for `&str` rustc guarantees validity anyway.
/// `meta` carries the element count for both str and `[u8; N]`, and is
/// clamped to the allocation size so `get_bytes_unchecked` stays in range.
pub fn str_const_bytes<'a>(tcx: TyCtxt<'a>, c: &Const<'a>) -> Option<(&'a [u8], u64)> {
    use rustc_abi::Size;
    use rustc_middle::mir::interpret::{AllocRange, GlobalAlloc, Scalar};
    let (cval, ty) = match c {
        Const::Val(v, ty) => (*v, *ty),
        _ => return None,
    };
    match ty.kind() {
        // `&str`, `&[u8; N]`, `&[u8]`, `&u8` — every fat/thin pointer to a
        // byte container whose payload the backend inlines.
        TyKind::Ref(_, inner, Mutability::Not) => match inner.kind() {
            TyKind::Str | TyKind::Slice(_) | TyKind::Array(..) => {}
            TyKind::Uint(ty::UintTy::U8) => {}
            _ => return None,
        },
        _ => return None,
    }
    // Three shapes carry a byte payload (nightly ConstValue):
    // - `Slice { alloc_id, meta }`: unsized pointee (`&str`, `&[u8]`), meta
    //   is the element count;
    // - `Scalar::Alloc(id)`: sized pointee (`&[u8; 7]`) — a thin pointer to
    //   the start of the allocation, length from the pointee's layout;
    // - anything else: no payload (ints, floats, ZeroSized).
    let pointee = match ty.kind() {
        TyKind::Ref(_, inner, _) => *inner,
        _ => return None,
    };
    let (alloc_id, start, want) = match cval {
        rustc_middle::mir::ConstValue::Slice { alloc_id, meta } => (alloc_id, Size::ZERO, meta),
        rustc_middle::mir::ConstValue::Scalar(s) => {
            // Thin pointer into a static allocation. `try_to_scalar_int`
            // yields `Err(Scalar<AllocId>)` for a relative-offset pointer,
            // which carries the AllocId and the byte offset.
            let (prov, offset) = match s.try_to_scalar_int() {
                Ok(_) => return None,               // plain integer, no payload
                Err(Scalar::Ptr(p, _)) => p.into_raw_parts(),
                Err(Scalar::Int(_)) => return None, // erased int, no payload
            };
            let typing_env = ty::TypingEnv::fully_monomorphized();
            let lay = tcx
                .layout_of(ty::PseudoCanonicalInput { typing_env, value: pointee })
                .ok()?;
            (prov, offset, lay.layout.size.bytes())
        }
        _ => return None,
    };
    let mem = match tcx.global_alloc(alloc_id) {
        GlobalAlloc::Memory(m) => m,
        _ => return None,
    };
    // Clamp to the real allocation: `get_bytes_unchecked` trusts the range.
    let avail = mem.inner().size().bytes().saturating_sub(start.bytes());
    let n = want.min(avail);
    let bytes = mem
        .inner()
        .get_bytes_unchecked(AllocRange { start, size: Size::from_bytes(n) });
    Some((bytes, n))
}

pub fn operand_ty_short<'tcx>(tcx: TyCtxt<'tcx>, body: &Body<'tcx>, op: &Operand<'tcx>) -> String {
    use rustc_middle::ty::{FloatTy, IntTy, UintTy};
    let ty = match op {
        Operand::Copy(p) | Operand::Move(p) => p.ty(&body.local_decls, tcx).ty,
        Operand::Constant(c) => match c.const_ {
            Const::Val(_, ty) => ty,
            Const::Ty(ty, _) => ty,
            _ => return "?".to_string(),
        },
        _ => return "?".to_string(),
    };
    match ty.kind() {
        TyKind::Uint(u) => match u {
            UintTy::U8 => "u8".to_string(), UintTy::U16 => "u16".to_string(),
            UintTy::U32 => "u32".to_string(), UintTy::U64 => "u64".to_string(),
            UintTy::U128 => "u128".to_string(), UintTy::Usize => "usize".to_string(),
        },
        TyKind::Int(i) => match i {
            IntTy::I8 => "i8".to_string(), IntTy::I16 => "i16".to_string(),
            IntTy::I32 => "i32".to_string(), IntTy::I64 => "i64".to_string(),
            IntTy::I128 => "i128".to_string(), IntTy::Isize => "isize".to_string(),
        },
        TyKind::Float(f) => match f {
            FloatTy::F16 => "f16".to_string(), FloatTy::F32 => "f32".to_string(),
            FloatTy::F64 => "f64".to_string(), FloatTy::F128 => "f128".to_string(),
        },
        TyKind::Bool => "bool".to_string(),
        TyKind::Char => "char".to_string(),
        TyKind::Ref(..) => "&".to_string(),
        TyKind::RawPtr(..) => "*".to_string(),
        _ => trunc(format!("{:?}", ty), 30),
    }
}

pub fn ty_sa_sig_opt(ty: Ty<'_>) -> Option<String> {
    use rustc_middle::ty::{FloatTy, IntTy, UintTy};
    if ty.is_unit() {
        return Some("void".to_string());
    }
    Some(
        match ty.kind() {
            TyKind::Uint(u) => match u {
                UintTy::U8 => "u8",
                UintTy::U16 => "u16",
                UintTy::U32 => "u32",
                UintTy::U64 => "u64",
                UintTy::Usize => "u64",
                UintTy::U128 => return None,
            },
            TyKind::Int(i) => match i {
                IntTy::I8 => "i8",
                IntTy::I16 => "i16",
                IntTy::I32 => "i32",
                IntTy::I64 => "i64",
                IntTy::Isize => "i64",
                IntTy::I128 => return None,
            },
            TyKind::Float(f) => match f {
                FloatTy::F16 | FloatTy::F32 => "f32",
                FloatTy::F64 | FloatTy::F128 => "f64",
            },
            TyKind::Bool => "u8",
            TyKind::Char => "u32",
            _ => "ptr",
        }
        .to_string(),
    )
}

/// True when a place's type is zero-sized (borrowing it needs no storage;
/// the value carries no data). Resolution failure -> false (loud elsewhere).
pub fn place_is_zst<'a>(tcx: TyCtxt<'a>, body: &Body<'a>, p: &Place<'a>) -> bool {
    use rustc_middle::ty::{PseudoCanonicalInput, TypingEnv};
    let ty = p.ty(&body.local_decls, tcx).ty;
    let env = TypingEnv::fully_monomorphized();
    tcx.layout_of(PseudoCanonicalInput { typing_env: env, value: ty })
        .map(|l| l.size.bytes() == 0)
        .unwrap_or(false)
}

/// Callee signature for a call operand: (param SA-tys, ret SA-ty).
/// FnDef (incl. trait-method desugars like `FnMut::call_mut`) resolves via
/// `fn_sig`; any unrepresentable part (128-bit ints) yields None and the
/// backend goes loud (no typed `@extern` can be declared).
pub fn fn_operand_sig(tcx: TyCtxt<'_>, op: &Operand<'_>) -> Option<(Vec<String>, String)> {
    fn sig_of(tcx: TyCtxt<'_>, def_id: &rustc_hir::def_id::DefId) -> Option<(Vec<String>, String)> {
        let sig = tcx.fn_sig(*def_id).instantiate_identity().skip_binder();
        let mut params = Vec::with_capacity(sig.inputs().len());
        for t in sig.inputs() {
            params.push(ty_sa_sig_opt(*t)?);
        }
        Some((params, ty_sa_sig_opt(sig.output())?))
    }
    if let Operand::Constant(c) = op {
        let c: &ConstOperand<'_> = c;
        // Call funcs arrive as ZST fn items: `Val(ZeroSized, FnDef(did, args))`.
        if let Const::Val(_, ty) = c.const_ {
            if let TyKind::FnDef(def_id, _args) = ty.kind() {
                return sig_of(tcx, def_id);
            }
        }
        if let Const::Ty(_, ct) = c.const_ {
            if let ConstKind::Value(ty::Value { ty, .. }) = ct.kind() {
                if let TyKind::FnDef(def_id, _args) = ty.kind() {
                    return sig_of(tcx, def_id);
                }
            }
        }
    }
    None
}

pub fn aggregate_layout<'tcx>(
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

/// Structured projection info for one MIR place (field-precision).
///
/// The driver used to collapse every place to its root local
/// (`place_name`), so a field read `_1.1` arrived in mir.json as plain
/// `_1` and mir2sa rendered the whole buffer pointer as the field value.
/// `place_proj` walks the projection chain under rustc layouts and
/// reports what mir2sa needs to render the place exactly:
///
/// - `off`: with no deref, the total field byte offset from the root
///   buffer; with a deref, the offset from the root buffer to the cell
///   holding the dereferenced pointer (0 when the deref is first).
/// - `off2`: trailing field offset after the deref (0 without one).
/// - `deref` / `deref_first`: the chain holds exactly one `Deref`, and
///   whether it is the first element.
/// - `pty`: SA kind of the projected place: a scalar name (`u64`,
///   `i32`, `u8`, `f64`, ...), `ptr` for a reference/raw-pointer/fn-ptr
///   value (loaded, not added), `buf` for an aggregate/array/tuple
///   value (address = base + offset), `zst` for a zero-sized value
///   (renders as 0). `None` for old/unknown shapes.
/// - `loud`: the chain cannot be resolved exactly (dynamic index,
///   subslice, opaque cast, multiple derefs, unresolvable layout,
///   unmappable final type). mir2sa must render such operands loud,
///   never as a plain root local.
///
/// Projectionless places return the default (no offset, no type, not
/// loud): they keep exactly today's rendering, including generic
/// locals whose layout cannot be resolved.
/// - `deref_last`: the projection's last step is a Deref (the value
///   is the whole pointee buffer, not a field cell): buf values bind
///   the deref resolution itself instead of loading a slot.
#[derive(Clone, Debug, Default)]
pub struct PlaceProj {
    pub off: u64,
    pub off2: u64,
    pub deref: bool,
    pub deref_first: bool,
    pub deref_last: bool,
    pub pty: Option<String>,
    pub loud: bool,
}

fn proj_layout<'tcx>(
    tcx: TyCtxt<'tcx>,
    ty: Ty<'tcx>,
) -> Option<rustc_middle::ty::layout::TyAndLayout<'tcx>> {
    let typing_env = ty::TypingEnv::fully_monomorphized();
    tcx.layout_of(ty::PseudoCanonicalInput { typing_env, value: ty })
        .ok()
}

pub fn place_proj<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &Body<'tcx>,
    place: &Place<'tcx>,
) -> PlaceProj {
    use rustc_middle::mir::ProjectionElem;
    use rustc_middle::ty::layout::LayoutCx;
    let mut out = PlaceProj::default();
    if place.projection.is_empty() {
        return out;
    }
    let mut cur_ty = body.local_decls[place.local].ty;
    let mut variant: Option<rustc_middle::ty::layout::TyAndLayout<'tcx>> = None;
    let mut seen_deref = false;
    // mir2sa stores an aggregate-typed field as a POINTER SLOT in the
    // outer buffer (lower_adt_init stores `^inner`, not inline bytes),
    // so a path that crosses such a field cannot use one total rustc
    // offset: the next step lives in a different buffer. Those paths
    // go loud instead of silently reading the wrong cell.
    let is_buf = |ty: Ty<'tcx>| {
        matches!(
            ty.kind(),
            TyKind::Adt(..) | TyKind::Tuple(..) | TyKind::Array(..)
        )
    };
    let elems: Vec<_> = place.iter_projections().map(|(_, e)| e).collect();
    let n = elems.len();
    let deref_last = elems
        .last()
        .is_some_and(|e| matches!(e, ProjectionElem::Deref));
    for (i, elem) in elems.into_iter().enumerate() {
        match elem {
            ProjectionElem::Field(idx, field_ty) => {
                let lay = match &variant {
                    Some(v) => Some(*v),
                    None => proj_layout(tcx, cur_ty),
                };
                let Some(lay) = lay else {
                    out.loud = true;
                    return out;
                };
                if !matches!(
                    lay.fields,
                    rustc_abi::FieldsShape::Arbitrary { .. }
                        | rustc_abi::FieldsShape::Array { .. }
                ) {
                    out.loud = true;
                    return out;
                }
                if idx.index() >= lay.fields.count() {
                    out.loud = true;
                    return out;
                }
                let o = lay.fields.offset(idx.index()).bytes();
                if seen_deref {
                    out.off2 += o;
                } else {
                    out.off += o;
                }
                variant = None;
                if is_buf(field_ty) && i + 1 < n {
                    out.loud = true;
                    return out;
                }
                cur_ty = field_ty;
            }
            ProjectionElem::Deref => {
                if seen_deref {
                    out.loud = true;
                    return out;
                }
                seen_deref = true;
                out.deref = true;
                out.deref_first = i == 0;
                cur_ty = match cur_ty.kind() {
                    TyKind::Ref(_, inner, _) | TyKind::RawPtr(inner, _) => *inner,
                    _ => {
                        out.loud = true;
                        return out;
                    }
                };
                variant = None;
            }
            ProjectionElem::Downcast(_, variant_idx) => {
                if !matches!(cur_ty.kind(), TyKind::Adt(..)) {
                    out.loud = true;
                    return out;
                }
                let Some(lay) = proj_layout(tcx, cur_ty) else {
                    out.loud = true;
                    return out;
                };
                let typing_env = ty::TypingEnv::fully_monomorphized();
                let cx = LayoutCx::new(tcx, typing_env);
                variant = Some(lay.for_variant(&cx, variant_idx));
            }
            ProjectionElem::ConstantIndex { .. } => {
                out.loud = true;
                return out;
            }
            _ => {
                out.loud = true;
                return out;
            }
        }
    }
    let Some(final_lay) = proj_layout(tcx, cur_ty) else {
        out.loud = true;
        return out;
    };
    if final_lay.size.bytes() == 0 {
        out.pty = Some("zst".to_string());
    } else {
        out.pty = match cur_ty.kind() {
            TyKind::Uint(_) | TyKind::Int(_) | TyKind::Float(_)
            | TyKind::Bool | TyKind::Char => ty_sa_sig_opt(cur_ty),
            TyKind::Ref(..) | TyKind::RawPtr(..) | TyKind::FnPtr(..) => {
                Some("ptr".to_string())
            }
            TyKind::Adt(..) | TyKind::Tuple(..) | TyKind::Array(..) => {
                Some("buf".to_string())
            }
            _ => None,
        };
        if out.pty.is_none() {
            out.loud = true;
        }
    }
    out.deref_last = deref_last;
    out
}
