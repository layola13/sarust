//! mir2sa::render_util.
use crate::const_util::{const_array_elem, const_elem_ty};
use crate::mir::{CallSig, Operand, Rvalue};

// ---------------------------------------------------------------------------
// lower: mir.json -> .sa
// ---------------------------------------------------------------------------

/// Sanitize a Rust symbol fragment into a legal SA identifier
/// (`[A-Za-z0-9_]`, non-empty, not starting with a digit): `{closure#0}` and
/// `{constant#0}` arrive verbatim from `def_path_str` short names.
pub fn sa_ident(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() {
        s.push_str("empty");
    }
    if s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        s.insert(0, '_');
    }
    s
}

/// Block labels must be `L_*` at column 0 (`bb0:` is ForbiddenSyntax).
pub fn sa_label(id: &str) -> String {
    format!("L_{}", sa_ident(id))
}

/// Flatten external text for full-line `//` comments (embedded newlines
/// would leak bare code lines, which is ForbiddenSyntax).
pub fn flat_comment(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// True when a Const operand cannot render parse-clean in a scalar
/// position: not decimalizable, not zero-sized, and not an aggregate-only
/// string payload (`str_bytes` materializes solely via ADT buffers).
/// Callers emit loud UNSUPPORTED instead of raw Debug text (which carries
/// `{}`/spaces that are ForbiddenSyntax in SA code positions).
pub fn const_needs_loud(op: &Operand) -> bool {
    match op {
        // Join-ambiguous uses (version.rs) always go loud.
        Operand::Conflict { .. } => true,
        Operand::Const { value, str_bytes, .. } => {
            if value.trim_start().starts_with("Val(ZeroSized") {
                return false;
            }
            if str_bytes.is_some() {
                return true;
            }
            const_scalar_text(value).is_none()
        }
        _ => false,
    }
}

/// Why an operand is loud, as a stable name: a join-ambiguous use
/// (`__VERSION_CONFLICT__` from version.rs) is a CONFLICT, not an unresolvable
/// const. Keeping them apart matters: the conflict class is the "needs phi"
/// family, and mislabelling it as a const problem hides the real blocker.
pub fn loud_const_kind(op: &Operand) -> Option<&'static str> {
    match op {
        Operand::Conflict { .. } => Some("Conflict"),
        Operand::Const { .. } => const_needs_loud(op).then_some("ConstValue"),
        _ => None,
    }
}

/// Loud reason text for one operand, `None` when it is fine.
pub fn loud_operand_reason(op: &Operand) -> Option<String> {
    match op {
        Operand::Conflict { place, .. } => {
            Some(format!("version conflict at join (multiple reaching defs of {})", place))
        }
        Operand::Const { value, .. } if const_needs_loud(op) => Some(format!(
            "unresolvable const {}",
            value.chars().take(60).collect::<String>()
        )),
        _ => None,
    }
}

/// True when a const operand in VALUE position (`_x = <const>`) is
/// materializable: sized byte-array literals get an inline buffer + thin
/// address (see layout::plan_const_bytes), so they are NOT loud there.
/// Every other position (call args, binop operands, discr/cond, aggregate
/// elems) still needs a register the const machinery does not build, so it
/// keeps the strict `const_needs_loud` verdict.
pub fn const_needs_loud_value(op: &Operand) -> bool {
    match op {
        Operand::Const { value, str_bytes, str_len, .. } => {
            let inlineable = crate::layout::plan_const_bytes(
                "_probe", "bb", 0, value, str_bytes.as_ref(), *str_len,
            )
            .is_some()
                || crate::layout::plan_str_pair(
                    "_probe", "bb", 0, value, str_bytes.as_ref(), *str_len,
                )
                .is_some();
            !inlineable && const_needs_loud(op)
        }
        _ => const_needs_loud(op),
    }
}

/// True when a const operand in CALL-ARGUMENT position needs loud
/// handling: same as `const_needs_loud`, except a slice literal the pair
/// planner can materialize (see `layout::plan_str_pair`) is fine — the
/// call site emits the byte buffer + (ptr,len) pair preamble and passes
/// the pair address as the single `ptr` argument. Scalar-position rules
/// are unchanged (a slice const in a binop operand, discr, or assert
/// cond is still loud: those positions take a plain scalar, not a pair).
pub fn const_needs_loud_call(op: &Operand) -> bool {
    match op {
        Operand::Const { value, str_bytes, str_len, .. } => {
            if crate::layout::plan_str_pair(
                "_probe", "bb", 0, value, str_bytes.as_ref(), *str_len,
            )
            .is_some()
            {
                return false;
            }
            const_needs_loud(op)
        }
        _ => const_needs_loud(op),
    }
}

/// Call-position loud kind, mirroring `loud_const_kind` but through the
/// call predicate (pair-materializable slice args are not loud).
pub fn loud_const_kind_call(op: &Operand) -> Option<&'static str> {
    match op {
        Operand::Conflict { .. } => Some("Conflict"),
        Operand::Const { .. } => const_needs_loud_call(op).then_some("ConstValue"),
        _ => None,
    }
}

/// Render one call argument, materializing a slice-literal const into a
/// preamble when possible. Returns (preamble lines, argument text,
/// byte-buffer registers to free after the call); the caller emits the
/// preamble before the call, uses the text inline, and frees each buffer
/// right after the call.
///
/// The post-call free is required, not cosmetic: the byte buffer is only
/// ever read by the pair-building `store`, never moved, so on a
/// normal-return path through a non-dominating block no later pass frees
/// it and `sa check` traps MemoryLeak at `return` (105, 56). The pair
/// itself is consumed by the `_carg` move and freed by borrow-end, so
/// only the byte buffer needs this. `exit_frees` skips registers already
/// freed on the path (its stale set), so entry-block calls do not
/// double-free. This mirrors the existing post-call frees of call-arg
/// temps (`!_carg_*`, `!_12` for a thin string buffer).
/// `idx` is the block's temp counter (shared with the Use arms, so pair
/// temps stay unique within the block).
pub fn render_call_arg_preamble(
    op: &Operand,
    bid: &str,
    idx: &mut usize,
) -> (Vec<String>, String, Vec<String>) {
    // Field/deref args load first; the temp is freed right after the
    // call (same post-call free list as the str-pair byte buffers).
    if operand_is_projected(op) {
        let (lines, tmp) = field_value_temp(op, bid, idx);
        return (lines, tmp.clone(), vec![tmp]);
    }
    match op {
        Operand::Const { value, str_bytes, str_len, .. } => {
            let tmp = format!("_carg_{}_{}", bid, *idx);
            if let Some(lines) = crate::layout::plan_str_pair(
                &tmp, bid, *idx, value, str_bytes.as_ref(), *str_len,
            ) {
                // plan_str_pair binds `tmp` as its last line; the whole
                // plan is the preamble and the arg is `tmp`. The byte
                // buffer is the plan's first allocation.
                let buf = format!("_sbuf_{}_{}", bid, *idx);
                *idx += 1;
                return (lines, tmp, vec![buf]);
            }
            (vec![], render_call_arg(op), vec![])
        }
        _ => (vec![], render_call_arg(op), vec![]),
    }
}

/// Short reason when an rvalue's scalar-position consts need loud handling.
/// Multi-element aggregates self-gate inside lower_array_init/lower_adt_init.
pub fn assign_loud_const(rv: &Rvalue) -> Option<String> {
    use crate::mir::Rvalue;
    let hit: Option<&Operand> = match rv {
        Rvalue::Use { op } => Some(op).filter(|o| const_needs_loud_value(o)),
        Rvalue::BinOp { left, right, .. } => {
            if const_needs_loud(left) {
                Some(left)
            } else if const_needs_loud(right) {
                Some(right)
            } else {
                None
            }
        }
        Rvalue::UnOp { operand, .. } => Some(operand.as_ref()).filter(|o| const_needs_loud(o)),
        Rvalue::Cast { op, .. } => Some(op.as_ref()).filter(|o| const_needs_loud(o)),
        Rvalue::Call { args, .. } => args.iter().find(|a| const_needs_loud_call(a)),
        Rvalue::Aggregate { elems, .. } if elems.len() <= 1 => {
            elems.first().filter(|e| matches!(e, Operand::Const { .. }) && const_needs_loud(e))
        }
        _ => None,
    };
    hit.map(|op| match op {
        Operand::Conflict { place, .. } => {
            format!("version conflict at join (multiple reaching defs of {})", place)
        }
        Operand::Const { value, .. } => format!("unresolvable const {}", value.chars().take(60).collect::<String>()),
        _ => "unresolvable const".to_string(),
    })
}

/// Deterministic numeric panic code for an assert failure in block `bid`
/// (`bb12` -> 1512; 1500-range avoids sa_std's 14xx family).
pub fn assert_panic_code(bid: &str) -> u32 {
    let n: u32 = bid
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(0);
    1500 + (n % 100)
}

/// Panic code for MIR `unreachable` (1600-range, distinct from asserts).
/// Bare `unreachable` ends the SA function textually (anything after it is
/// ForbiddenSyntax); `panic` diverges without ending it, so unreachable
/// blocks lower to loud aborts and siblings keep assembling.
pub fn unreachable_panic_code(bid: &str) -> u32 {
    let n: u32 = bid
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(0);
    1600 + (n % 100)
}

/// Decimal text for a scalar const value (`Val(Scalar(0x..), TY)` or `N_TY`
/// or bare bool), or None for non-scalars (slices, unevaluated, ZST…) and
/// floats (no verified float-literal shape yet — stays loud, never guessed).
pub fn const_scalar_text(value: &str) -> Option<String> {
    let v = value.trim();
    if v == "true" {
        return Some("1".to_string());
    }
    if v == "false" {
        return Some("0".to_string());
    }
    let (ty, size) = const_elem_ty(v)?;
    if matches!(ty, "f32" | "f64") {
        return None;
    }
    const_array_elem(v, ty, size)
}

pub fn render_operand(op: &Operand) -> String {
    // NOTE: MIR Move renders as a PLAIN place here. SA `=` already moves
    // (`_x = ^_y` is UnknownRegister); `^` is legal only in call args
    // (render_call_arg) and store values. See sala 03_sa_asm/02_sa_syntax.
    match op {
        Operand::Move { place , .. } => place.clone(),
        Operand::Copy { place , .. } => place.clone(),
        // Indexed reads never reach here either: every code position
        // checks operand_is_projected / operand_proj_loud first and
        // routes through index_value_into; only loud-fallback comment
        // strings can still print an indexed operand's root.
        Operand::Index { place, .. } => place.clone(),
        // Conflict sentinels never reach here (intercepted loud upstream);
        // the fallback keeps rendering total.
        Operand::Conflict { place, .. } => place.clone(),
        Operand::Const { value, .. } => {
            // Zero-sized values carry no data: bind a null marker (exact).
            if value.trim_start().starts_with("Val(ZeroSized") {
                return "0".to_string();
            }
            const_scalar_text(value).unwrap_or_else(|| value.clone())
        }
    }
}

/// Call arguments render PLAIN (verified): `^` in call position mismatches
/// plain-param `@extern` decls (CapabilityMismatch); our generated decls use
/// plain params, so args must be plain. (The docs' `call @f(^x)` shape only
/// applies when the callee declares `^` params.)
/// Uses render_operand directly; this alias documents the rule.
pub fn render_call_arg(op: &Operand) -> String {
    match op {
        Operand::Move { place , .. } => place.clone(),
        _ => render_operand(op),
    }
}

/// Projection view of a Move/Copy operand: the driver's structured
/// place info (mir.rs), or None for Const/Conflict operands.
pub struct OperandProj<'a> {
    pub place: &'a str,
    pub off: u64,
    pub off2: u64,
    pub deref: bool,
    pub deref_first: bool,
    pub deref_last: bool,
    pub pty: Option<&'a str>,
    pub proj_loud: bool,
}

/// Index projection view, shared by Operand::Index and by Ref/RawPtr
/// rvalues carrying index fields. `place` is the root local; off/off2
/// are the static field offsets (pre/post deref) exactly as for
/// Copy/Move; exactly one of idx / idx_const / idx_fe selects the
/// index form (see mir.rs). `fat` marks a fat-pointer deref in the
/// chain (data pointer at pair +0, length at +8).
pub struct IndexProj<'a> {
    pub place: &'a str,
    pub off: u64,
    pub off2: u64,
    pub deref: bool,
    pub deref_first: bool,
    pub pty: Option<&'a str>,
    pub proj_loud: bool,
    pub idx: Option<&'a str>,
    pub idx_size: u64,
    pub idx_const: Option<u64>,
    pub idx_fe: Option<u64>,
    pub fat: bool,
}

/// The index form of an IndexProj: dynamic local + stride, static
/// byte offset, or slice from-end offset + stride.
pub enum IndexForm<'a> {
    Dyn(&'a str, u64),
    Const(u64),
    FromEnd(u64, u64),
}

impl<'a> IndexProj<'a> {
    pub fn form(&self) -> Option<IndexForm<'a>> {
        match (self.idx, self.idx_const, self.idx_fe) {
            (Some(l), None, None) => Some(IndexForm::Dyn(l, self.idx_size)),
            (None, Some(b), None) => Some(IndexForm::Const(b)),
            (None, None, Some(o)) => Some(IndexForm::FromEnd(o, self.idx_size)),
            _ => None,
        }
    }
}

pub fn operand_index(op: &Operand) -> Option<IndexProj<'_>> {
    match op {
        Operand::Index { place, off, off2, deref, deref_first, pty, proj_loud, idx, idx_size, idx_const, idx_fe, fat } => {
            Some(IndexProj {
                place,
                off: *off,
                off2: *off2,
                deref: *deref,
                deref_first: *deref_first,
                pty: pty.as_deref(),
                proj_loud: *proj_loud,
                idx: idx.as_deref(),
                idx_size: *idx_size,
                idx_const: *idx_const,
                idx_fe: *idx_fe,
                fat: *fat,
            })
        }
        _ => None,
    }
}

/// Index view of a Ref/RawPtr rvalue, Some only when an index form is
/// present (a plain fat deref without an index keeps the old path:
/// its address IS the pair address, not the data pointer).
pub fn rvalue_index(rv: &Rvalue) -> Option<IndexProj<'_>> {
    match rv {
        Rvalue::Ref { place, off, off2, deref, deref_first, pty, proj_loud, idx, idx_size, idx_const, idx_fe, fat, .. } => {
            if idx.is_some() || idx_const.is_some() || idx_fe.is_some() {
                Some(IndexProj {
                    place,
                    off: *off,
                    off2: *off2,
                    deref: *deref,
                    deref_first: *deref_first,
                    pty: pty.as_deref(),
                    proj_loud: *proj_loud,
                    idx: idx.as_deref(),
                    idx_size: *idx_size,
                    idx_const: *idx_const,
                    idx_fe: *idx_fe,
                    fat: *fat,
                })
            } else {
                None
            }
        }
        Rvalue::RawPtr { place, off, off2, deref, deref_first, pty, proj_loud, idx, idx_size, idx_const, idx_fe, fat, .. } => {
            if idx.is_some() || idx_const.is_some() || idx_fe.is_some() {
                Some(IndexProj {
                    place,
                    off: *off,
                    off2: *off2,
                    deref: *deref,
                    deref_first: *deref_first,
                    pty: pty.as_deref(),
                    proj_loud: *proj_loud,
                    idx: idx.as_deref(),
                    idx_size: *idx_size,
                    idx_const: *idx_const,
                    idx_fe: *idx_fe,
                    fat: *fat,
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

pub fn operand_proj(op: &Operand) -> Option<OperandProj<'_>> {
    match op {
        Operand::Move { place, off, off2, deref, deref_first, deref_last, pty, proj_loud, .. }
        | Operand::Copy { place, off, off2, deref, deref_first, deref_last, pty, proj_loud, .. } => {
            Some(OperandProj {
                place,
                off: *off,
                off2: *off2,
                deref: *deref,
                deref_first: *deref_first,
                deref_last: *deref_last,
                pty: pty.as_deref(),
                proj_loud: *proj_loud,
            })
        }
        _ => None,
    }
}

/// True when the operand reads through a projection (a field offset,
/// a deref, or an aggregate-typed field even at offset 0): it must
/// render as a load/add, never as its root local. Scalar field-0
/// (pty scalar, off 0) is deliberately NOT projected: collapsed-pair
/// .0 reads (AddWithOverflow results) hold values, not buffers.
/// Field-only projection: true when the operand reads a field cell
/// (non-zero offset) or an aggregate-typed field (pointer slot). Pure
/// deref reads (`*p`, off 0, scalar/ptr type) are NOT field projections:
/// they behave like whole-value copies for use-counting (spill slots,
/// borrow ends, asm gates), and excluding them lost baseline slots
/// (313: `_7` lost its slot and its dereferenced value conflicted at
/// the join). Rendering still uses operand_is_projected (which includes
/// deref) for field loads.
pub fn operand_is_field_projected(op: &Operand) -> bool {
    // An indexed read loads from a buffer cell and never consumes its
    // base, exactly like a field read (borrow_end skips it as a
    // consuming event for the base; the index local itself is a scalar
    // read by its own bounds check through plain operands).
    if operand_index(op).is_some() {
        return true;
    }
    match op {
        Operand::Move { off, pty, .. } | Operand::Copy { off, pty, .. } => {
            *off != 0 || pty.as_deref() == Some("buf")
        }
        _ => false,
    }
}

pub fn operand_is_projected(op: &Operand) -> bool {
    // An index read never binds its root local: even index 0 must
    // load the cell (a plain move would bind the buffer pointer).
    if operand_index(op).is_some() {
        return true;
    }
    operand_proj(op).is_some_and(|p| p.off != 0 || p.deref || p.pty == Some("buf"))
}

/// True when the operand's projection cannot be rendered exactly: the
/// driver flagged it, or it is projected without a field type (old or
/// hand-written mir.json cannot type the load). Such operands go loud.
/// True when an indexed place cannot be rendered exactly: the driver
/// flagged it, the index form is missing or ambiguous (two forms at
/// once), the element type is unknown or aggregate-typed (inline
/// bytes vs ADT pointer slots share one pty, so neither is exact),
/// a fat flag appears without a deref, or a from-end index lacks the
/// fat length it computes from.
pub fn index_proj_loud(pr: &IndexProj<'_>) -> bool {
    if pr.proj_loud {
        return true;
    }
    if pr.form().is_none() {
        return true;
    }
    match pr.pty {
        None => return true,
        Some("buf") => return true,
        _ => {}
    }
    if pr.fat && !pr.deref {
        return true;
    }
    if pr.idx_fe.is_some() && !pr.fat {
        return true;
    }
    false
}

pub fn operand_proj_loud(op: &Operand) -> bool {
    if let Some(ipr) = operand_index(op) {
        return index_proj_loud(&ipr);
    }
    match operand_proj(op) {
        Some(pr) => pr.proj_loud || (operand_is_projected(op) && pr.pty.is_none()),
        None => false,
    }
}

/// True when any operand inside an rvalue carries an unrenderable
/// projection (checked by lower() before rendering, and mirrored by
/// coverage, so a loud count can never drift from the emitted code).
pub fn rvalue_proj_loud(rv: &Rvalue) -> bool {
    match rv {
        Rvalue::Use { op } => operand_proj_loud(op),
        Rvalue::BinOp { left, right, .. } => {
            operand_proj_loud(left) || operand_proj_loud(right)
        }
        Rvalue::UnOp { operand, .. } => operand_proj_loud(operand),
        Rvalue::Cast { op, .. } => operand_proj_loud(op.as_ref()),
        Rvalue::Call { args, .. } => args.iter().any(operand_proj_loud),
        Rvalue::Aggregate { elems, .. } => elems.iter().any(operand_proj_loud),
        Rvalue::Repeat { op, .. } => operand_proj_loud(op.as_ref()),
        Rvalue::Ref { proj_loud, off, deref, pty, .. } => {
            if let Some(ipr) = rvalue_index(rv) {
                return index_proj_loud(&ipr);
            }
            *proj_loud || ((*off != 0 || *deref) && pty.is_none())
        }
        Rvalue::RawPtr { proj_loud, off, deref, pty, .. } => {
            if let Some(ipr) = rvalue_index(rv) {
                return index_proj_loud(&ipr);
            }
            *proj_loud || ((*off != 0 || *deref) && pty.is_none())
        }
        _ => false,
    }
}

/// True for a long-lived aggregate-field Copy: a Use (or single-elem
/// aggregate) whose Copy operand reads an aggregate-typed field
/// (`pty == "buf"`) through a projection. Sharing the buffer would
/// alias source and dest under later place stores, and duplicating it
/// needs the inner layout, so it goes loud (Jev: aggcopy_loud).
/// A pure trailing deref is NOT a field copy: it copies the whole
/// pointee buffer (Clone's *self), shared like any whole-aggregate
/// Copy. Transient positions (BinOp/Call args) share like
/// whole-aggregate values already do, and are not covered here.
pub fn rvalue_agg_field_copy_loud(rv: &Rvalue) -> bool {
    let is_buf_copy = |op: &Operand| -> bool {
        match operand_proj(op) {
            // pty == "buf" already implies a projection (projectionless
            // places carry no pty), so a buf Copy is a field copy
            // unless it is a pure trailing deref (whole pointee value).
            Some(pr) => {
                matches!(op, Operand::Copy { .. })
                    && pr.pty == Some("buf")
                    && !(pr.deref && pr.deref_last)
            }
            None => false,
        }
    };
    match rv {
        Rvalue::Use { op } => is_buf_copy(op),
        Rvalue::Aggregate { elems, .. } if elems.len() == 1 => {
            is_buf_copy(&elems[0])
        }
        _ => false,
    }
}

/// (preamble, buffer pointer, offset) for a projected operand: the
/// pointer whose buffer the field offset applies to. No deref: the
/// root reg itself (an aggregate buffer pointer). Deref-first: the
/// root reg itself (it already holds the pointee buffer address).
/// Deref after leading fields: the pointer cell is loaded first.
/// Loads read the root reg without consuming it, so spilled and
/// unspilled bases behave identically here.
fn field_base(
    pr: &OperandProj<'_>,
    bid: &str,
    idx: &mut usize,
) -> (Vec<String>, String, u64) {
    if !pr.deref {
        (vec![], pr.place.to_string(), pr.off)
    } else if pr.deref_first {
        (vec![], pr.place.to_string(), pr.off2)
    } else {
        let tmp = format!("_mv_{}_{}", bid, *idx);
        *idx += 1;
        (
            vec![format!("{} = load {}+{} as ptr", tmp, pr.place, pr.off)],
            tmp,
            pr.off2,
        )
    }
}

/// (lines, data_reg, pair_reg, static_tail) for an indexed place:
/// `data_reg` holds the element array's data pointer; `pair_reg` is
/// Some only for fat derefs (the (ptr, len) pair address, which
/// from-end indices read their length from); `static_tail` is the
/// static byte offset still to add (off with no deref, off2 after a
/// deref). Loads read without consuming, so spilled and unspilled
/// bases behave identically here, as for field_base.
fn index_base(
    pr: &IndexProj<'_>,
    bid: &str,
    idx: &mut usize,
) -> (Vec<String>, String, Option<String>, u64) {
    if !pr.deref {
        (vec![], pr.place.to_string(), None, pr.off)
    } else if pr.deref_first {
        if pr.fat {
            let data = format!("_mv_{}_{}", bid, *idx);
            *idx += 1;
            (
                vec![format!("{} = load {}+0 as ptr", data, pr.place)],
                data,
                Some(pr.place.to_string()),
                pr.off2,
            )
        } else {
            (vec![], pr.place.to_string(), None, pr.off2)
        }
    } else {
        let p = format!("_mv_{}_{}", bid, *idx);
        *idx += 1;
        let mut lines = vec![format!("{} = load {}+{} as ptr", p, pr.place, pr.off)];
        if pr.fat {
            let data = format!("_mv_{}_{}", bid, *idx);
            *idx += 1;
            lines.push(format!("{} = load {}+0 as ptr", data, p));
            (lines, data, Some(p), pr.off2)
        } else {
            (lines, p, None, pr.off2)
        }
    }
}

/// (lines, addr_reg, addr_is_fresh) for an indexed place's element
/// address: `addr_reg` holds base + static_tail + index * stride.
/// When `addr_is_fresh` is false, `addr_reg` IS the base register
/// itself (zero total offset, no resolution temps) and callers must
/// bind it with `add base, 0`, never with a plain move.
fn index_addr_reg(
    pr: &IndexProj<'_>,
    bid: &str,
    idx: &mut usize,
) -> (Vec<String>, String, bool) {
    let (mut lines, data, pair, tail) = index_base(pr, bid, idx);
    let had_temps = !lines.is_empty();
    match pr.form() {
        Some(IndexForm::Dyn(local, size)) => {
            if size == 0 {
                (lines, data, had_temps)
            } else {
                let sc = format!("_mv_{}_{}", bid, *idx);
                *idx += 1;
                lines.push(format!("{} = mul {}, {}", sc, local, size));
                let mut addr = format!("_mv_{}_{}", bid, *idx);
                *idx += 1;
                lines.push(format!("{} = add {}, {}", addr, data, sc));
                if tail != 0 {
                    let a2 = format!("_mv_{}_{}", bid, *idx);
                    *idx += 1;
                    lines.push(format!("{} = add {}, {}", a2, addr, tail));
                    addr = a2;
                }
                (lines, addr, true)
            }
        }
        Some(IndexForm::Const(bytes)) => {
            if tail + bytes == 0 {
                (lines, data, had_temps)
            } else {
                let addr = format!("_mv_{}_{}", bid, *idx);
                *idx += 1;
                lines.push(format!("{} = add {}, {}", addr, data, tail + bytes));
                (lines, addr, true)
            }
        }
        Some(IndexForm::FromEnd(off, size)) => {
            // The loud gate guarantees a fat pair here; the fallback
            // keeps rendering total for direct (non-pipeline) callers.
            let Some(pair) = pair else {
                return (lines, data, had_temps);
            };
            let lent = format!("_mv_{}_{}", bid, *idx);
            *idx += 1;
            lines.push(format!("{} = load {}+8 as u64", lent, pair));
            let eff = format!("_mv_{}_{}", bid, *idx);
            *idx += 1;
            lines.push(format!("{} = sub {}, {}", eff, lent, off));
            if size == 0 {
                (lines, data, had_temps)
            } else {
                let sc = format!("_mv_{}_{}", bid, *idx);
                *idx += 1;
                lines.push(format!("{} = mul {}, {}", sc, eff, size));
                let mut addr = format!("_mv_{}_{}", bid, *idx);
                *idx += 1;
                lines.push(format!("{} = add {}, {}", addr, data, sc));
                if tail != 0 {
                    let a2 = format!("_mv_{}_{}", bid, *idx);
                    *idx += 1;
                    lines.push(format!("{} = add {}, {}", a2, addr, tail));
                    addr = a2;
                }
                (lines, addr, true)
            }
        }
        None => (lines, data, had_temps),
    }
}

/// Lines binding `dest` to an indexed operand's VALUE. Scalar and
/// pointer elements load from the computed element address;
/// zero-sized elements are 0 (their address is never read).
pub fn index_value_into(
    pr: &IndexProj<'_>,
    dest: &str,
    bid: &str,
    idx: &mut usize,
) -> Vec<String> {
    if pr.pty == Some("zst") {
        return vec![format!("{} = 0", dest)];
    }
    // Untyped / aggregate-typed index reads are gated loud upstream
    // (index_proj_loud); the fallback keeps rendering total.
    let Some(ty) = pr.pty else {
        return vec![format!("{} = 0", dest)];
    };
    if ty == "buf" {
        return vec![format!("{} = 0", dest)];
    }
    // Loads never consume their base, so a zero-offset address that
    // IS the base register loads exactly like a fresh address temp.
    let (mut lines, addr, _fresh) = index_addr_reg(pr, bid, idx);
    lines.push(format!("{} = load {}+0 as {}", dest, addr, ty));
    lines
}

/// Lines binding `dest` to an indexed Ref/RawPtr's ADDRESS. The
/// address temp is fresh, so binding it never moves the base; the
/// only base-preserving case is a zero total offset with no
/// resolution temps, which renders as `add base, 0` (a plain assign
/// would move the base pointer, but a borrow must not consume it).
pub fn index_addr_into(
    pr: &IndexProj<'_>,
    dest: &str,
    bid: &str,
    idx: &mut usize,
) -> Vec<String> {
    let (mut lines, addr, fresh) = index_addr_reg(pr, bid, idx);
    if fresh {
        lines.push(format!("{} = {}", dest, addr));
    } else {
        lines.push(format!("{} = add {}, 0", dest, addr));
    }
    lines
}

/// Same value rule as index_value_into but with a caller-chosen temp
/// name and no shared counter: temps derive from `tmp` itself, so
/// names stay unique per caller context. Used by lower_adt_init,
/// which has no mv_idx to thread.
pub fn index_value_named(pr: &IndexProj<'_>, tmp: &str) -> Vec<String> {
    if pr.pty == Some("zst") {
        return vec![format!("{} = 0", tmp)];
    }
    let Some(ty) = pr.pty else {
        return vec![format!("{} = 0", tmp)];
    };
    if ty == "buf" {
        return vec![format!("{} = 0", tmp)];
    }
    // Named-base resolution (mirrors index_base, temps from `tmp`).
    let (mut lines, data, pair, tail) = if !pr.deref {
        (vec![], pr.place.to_string(), None, pr.off)
    } else if pr.deref_first {
        if pr.fat {
            let data = format!("{}_data", tmp);
            (
                vec![format!("{} = load {}+0 as ptr", data, pr.place)],
                data,
                Some(pr.place.to_string()),
                pr.off2,
            )
        } else {
            (vec![], pr.place.to_string(), None, pr.off2)
        }
    } else {
        let p = format!("{}_ptr", tmp);
        let mut lines = vec![format!("{} = load {}+{} as ptr", p, pr.place, pr.off)];
        if pr.fat {
            let data = format!("{}_data", tmp);
            lines.push(format!("{} = load {}+0 as ptr", data, p));
            (lines, data, Some(p), pr.off2)
        } else {
            (lines, p, None, pr.off2)
        }
    };
    match pr.form() {
        Some(IndexForm::Dyn(local, size)) => {
            if size == 0 {
                lines.push(format!("{} = load {}+{} as {}", tmp, data, tail, ty));
            } else {
                let sc = format!("{}_sc", tmp);
                lines.push(format!("{} = mul {}, {}", sc, local, size));
                let mut addr = format!("{}_addr", tmp);
                lines.push(format!("{} = add {}, {}", addr, data, sc));
                if tail != 0 {
                    let a2 = format!("{}_addr2", tmp);
                    lines.push(format!("{} = add {}, {}", a2, addr, tail));
                    addr = a2;
                }
                lines.push(format!("{} = load {}+0 as {}", tmp, addr, ty));
            }
        }
        Some(IndexForm::Const(bytes)) => {
            lines.push(format!("{} = load {}+{} as {}", tmp, data, tail + bytes, ty));
        }
        Some(IndexForm::FromEnd(off, size)) => {
            let Some(pair) = pair else {
                return vec![format!("{} = 0", tmp)];
            };
            let lent = format!("{}_len", tmp);
            lines.push(format!("{} = load {}+8 as u64", lent, pair));
            let eff = format!("{}_eff", tmp);
            lines.push(format!("{} = sub {}, {}", eff, lent, off));
            if size == 0 {
                lines.push(format!("{} = load {}+{} as {}", tmp, data, tail, ty));
            } else {
                let sc = format!("{}_sc", tmp);
                lines.push(format!("{} = mul {}, {}", sc, eff, size));
                let mut addr = format!("{}_addr", tmp);
                lines.push(format!("{} = add {}, {}", addr, data, sc));
                if tail != 0 {
                    let a2 = format!("{}_addr2", tmp);
                    lines.push(format!("{} = add {}, {}", a2, addr, tail));
                    addr = a2;
                }
                lines.push(format!("{} = load {}+0 as {}", tmp, addr, ty));
            }
        }
        None => lines.push(format!("{} = 0", tmp)),
    }
    lines
}

/// Lines binding `dest` to a projected operand's VALUE. mir2sa stores
/// aggregate-typed fields as pointer slots (lower_adt_init stores the
/// inner buffer pointer, not inline bytes), so every field value is a
/// load: scalars by their driver type, reference/aggregate fields as
/// `ptr`, zero-sized values as 0.
pub fn field_value_into(
    op: &Operand,
    dest: &str,
    bid: &str,
    idx: &mut usize,
) -> Vec<String> {
    if let Some(ipr) = operand_index(op) {
        return index_value_into(&ipr, dest, bid, idx);
    }
    let pr = match operand_proj(op) {
        Some(pr) => pr,
        None => return vec![format!("{} = {}", dest, render_operand(op))],
    };
    let (mut lines, bp, off) = field_base(&pr, bid, idx);
    match pr.pty {
        Some("zst") => lines.push(format!("{} = 0", dest)),
        Some("buf") => {
            // A buf value reached through a trailing deref IS the
            // pointee buffer itself (the deref resolution), not a slot
            // load: Clone's whole-struct deref copy shares the buffer.
            if pr.deref && pr.deref_last {
                lines.push(format!("{} = {}", dest, bp))
            } else {
                lines.push(format!("{} = load {}+{} as ptr", dest, bp, off))
            }
        }
        Some("ptr") => {
            lines.push(format!("{} = load {}+{} as ptr", dest, bp, off))
        }
        Some(ty) => lines.push(format!("{} = load {}+{} as {}", dest, bp, off, ty)),
        None => lines.push(format!("{} = {}", dest, bp)),
    }
    lines
}

/// (preamble, temp) binding a fresh temp to a projected operand's
/// value; the temp is a plain read result (loads/adds never consume
/// the base), so Move and Copy share this path.
pub fn field_value_temp(
    op: &Operand,
    bid: &str,
    idx: &mut usize,
) -> (Vec<String>, String) {
    let tmp = format!("_mv_{}_{}", bid, *idx);
    *idx += 1;
    let lines = field_value_into(op, &tmp, bid, idx);
    (lines, tmp)
}

/// Same value rule as field_value_into but with a caller-chosen temp
/// name and no shared counter: the deref-after-fields pointer temp
/// derives from `tmp` itself, so names stay unique per caller context.
/// Used by lower_adt_init, which has no mv_idx to thread.
pub fn field_value_named(op: &Operand, tmp: &str) -> Vec<String> {
    if let Some(ipr) = operand_index(op) {
        return index_value_named(&ipr, tmp);
    }
    let pr = match operand_proj(op) {
        Some(pr) => pr,
        None => return vec![format!("{} = {}", tmp, render_operand(op))],
    };
    let mut lines = vec![];
    let (bp, off) = if !pr.deref {
        (pr.place.to_string(), pr.off)
    } else if pr.deref_first {
        (pr.place.to_string(), pr.off2)
    } else {
        let dt = format!("{}_deref", tmp);
        lines.push(format!("{} = load {}+{} as ptr", dt, pr.place, pr.off));
        (dt, pr.off2)
    };
    match pr.pty {
        Some("zst") => lines.push(format!("{} = 0", tmp)),
        Some("buf") => {
            if pr.deref && pr.deref_last {
                lines.push(format!("{} = {}", tmp, bp))
            } else {
                lines.push(format!("{} = load {}+{} as ptr", tmp, bp, off))
            }
        }
        Some("ptr") => {
            lines.push(format!("{} = load {}+{} as ptr", tmp, bp, off))
        }
        Some(ty) => lines.push(format!("{} = load {}+{} as {}", tmp, bp, off, ty)),
        None => lines.push(format!("{} = {}", tmp, bp)),
    }
    lines
}

/// Lines binding `dest` to a projected Ref/RawPtr's ADDRESS. Scalar
/// and reference fields live in their cell, so the address is
/// `base + off` (always an `add`, even at offset 0: a plain assign
/// would move the base pointer, but a borrow must not consume it).
/// Aggregate fields live in a separate buffer whose address is the
/// pointer stored in the slot, so the address is loaded.
pub fn field_addr_into(
    op_place: &str,
    off: u64,
    off2: u64,
    deref: bool,
    deref_first: bool,
    deref_last: bool,
    pty: Option<&str>,
    dest: &str,
    bid: &str,
    idx: &mut usize,
) -> Vec<String> {
    let pr = OperandProj {
        place: op_place,
        off,
        off2,
        deref,
        deref_first,
        deref_last,
        pty,
        proj_loud: false,
    };
    let (mut lines, bp, o) = field_base(&pr, bid, idx);
    if pty == Some("buf") {
        // Trailing deref: the address IS the deref resolution itself.
        if deref && deref_last {
            lines.push(format!("{} = {}", dest, bp));
        } else {
            lines.push(format!("{} = load {}+{} as ptr", dest, bp, o));
        }
    } else {
        lines.push(format!("{} = add {}, {}", dest, bp, o));
    }
    lines
}

/// MIR BinOp name -> SA mnemonic for the sign-agnostic subset.
/// Div/Rem/Shr/ordered-comparisons need signedness (driver-supplied);
/// anything else returns None (caller stays loud).
pub fn binop_mnemonic(op: &str) -> Option<&'static str> {
    match op {
        "Add" => Some("add"),
        "Sub" => Some("sub"),
        "Mul" => Some("mul"),
        "BitAnd" => Some("and"),
        "BitOr" => Some("or"),
        "BitXor" => Some("xor"),
        "Shl" => Some("shl"),
        "Eq" => Some("eq"),
        "Ne" => Some("ne"),
        _ => None,
    }
}

/// Signedness of a MIR integer type name: true = signed, false = unsigned.
/// None for everything SA cannot order/divide as a scalar int here:
/// floats (fcmp_* is a different family), bool, pointers/references,
/// aggregates, and 128-bit ints (no SA slot type).
pub fn int_type_signedness(ty: &str) -> Option<bool> {
    match ty.trim() {
        "i8" | "i16" | "i32" | "i64" | "isize" => Some(true),
        "u8" | "u16" | "u32" | "u64" | "usize" => Some(false),
        _ => None,
    }
}

/// Base local for a (possibly versioned) place: `_3_v2` -> `_3`.
/// None unless the place is a plain local (`_N`, optionally versioned).
fn base_local(place: &str) -> Option<&str> {
    let p = place.strip_prefix('_')?;
    let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = &p[digits.len()..];
    if rest.is_empty() {
        return Some(place);
    }
    if let Some(v) = rest.strip_prefix("_v") {
        if !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()) {
            return Some(&place[..1 + digits.len()]);
        }
        return None;
    }
    None
}

/// Signedness of one BinOp operand from MIR type info: a Const's own
/// type suffix, or a Move/Copy place's `local_types` entry (version
/// suffix stripped). None when the type is unknown or not a scalar int.
pub fn binop_operand_signedness(
    op: &Operand,
    local_types: &std::collections::BTreeMap<String, String>,
) -> Option<bool> {
    match op {
        Operand::Move { place , .. } | Operand::Copy { place , .. } => {
            let base = base_local(place)?;
            int_type_signedness(local_types.get(base)?)
        }
        // An indexed element's signedness is its own driver type
        // (the base place's array type says nothing about it).
        Operand::Index { pty, .. } => pty.as_deref().and_then(int_type_signedness),
        Operand::Const { value, .. } => {
            let (ty, _) = crate::const_util::const_elem_ty(value)?;
            int_type_signedness(ty)
        }
        Operand::Conflict { .. } => None,
    }
}

/// MIR ordered/div/rem/shift op -> SA mnemonic for a known signedness.
/// None for ops outside this family (callers try the sign-agnostic
/// table first).
pub fn signed_binop_mnemonic(op: &str, signed: bool) -> Option<&'static str> {
    match (op, signed) {
        ("Lt", true) => Some("slt"),
        ("Lt", false) => Some("ult"),
        ("Le", true) => Some("sle"),
        ("Le", false) => Some("ule"),
        ("Gt", true) => Some("sgt"),
        ("Gt", false) => Some("ugt"),
        ("Ge", true) => Some("sge"),
        ("Ge", false) => Some("uge"),
        ("Div", true) => Some("sdiv"),
        ("Div", false) => Some("udiv"),
        ("Rem", true) => Some("srem"),
        ("Rem", false) => Some("urem"),
        ("Shr", true) => Some("ashr"),
        ("Shr", false) => Some("lshr"),
        _ => None,
    }
}

/// Signedness-resolved mnemonic for a BinOp, or None when the op is
/// outside the signed family or its signedness cannot be established.
/// Shr reads only its left operand (the right one is a shift amount,
/// whose type never affects the shift kind); every other op in the
/// family needs both operands to resolve to the same signedness
/// (Rust BinOp operands share one type -- a mismatch means the type
/// info is inconsistent, so stay loud rather than guess).
pub fn signed_binop_for(
    op: &str,
    left: &Operand,
    right: &Operand,
    local_types: &std::collections::BTreeMap<String, String>,
) -> Option<&'static str> {
    if op == "Shr" {
        let s = binop_operand_signedness(left, local_types)?;
        return signed_binop_mnemonic(op, s);
    }
    let l = binop_operand_signedness(left, local_types)?;
    let r = binop_operand_signedness(right, local_types)?;
    if l != r {
        return None;
    }
    signed_binop_mnemonic(op, l)
}

/// True when render_rvalue can emit a real instruction for this BinOp:
/// the sign-agnostic table, a checked-arith helper, or a signedness-
/// resolved mnemonic. Shared by render and coverage so the loud count
/// can never drift from what lower() actually emits.
pub fn binop_emittable(
    op: &str,
    left: &Operand,
    right: &Operand,
    local_types: &std::collections::BTreeMap<String, String>,
) -> bool {
    binop_mnemonic(op).is_some()
        || checked_arith_helper(op).is_some()
        || signed_binop_for(op, left, right, local_types).is_some()
}

/// `*WithOverflow` -> the `sci/sa_std` checked helper that returns the exact
/// value and traps on overflow. None for every other binop.
pub fn checked_arith_helper(op: &str) -> Option<&'static str> {
    match op {
        "AddWithOverflow" => Some("sa_num_add_checked"),
        "SubWithOverflow" => Some("sa_num_sub_checked"),
        "MulWithOverflow" => Some("sa_num_mul_checked"),
        _ => None,
    }
}

/// True for rustc's overflow assert (`msg` starts with `Overflow(`), whose
/// check the sa_std helper already performs: the flag test is folded.
pub fn is_overflow_assert(msg: &str) -> bool {
    msg.trim_start().starts_with("Overflow(")
}

/// MIR UnOp name -> SA mnemonic (`r = op a` shape, probed legal).
pub fn unop_mnemonic(op: &str) -> Option<&'static str> {
    match op {
        "Not" => Some("not"),
        "Neg" => Some("neg"),
        _ => None,
    }
}

/// Loud verdict for a UnOp, shared by lower and coverage (parity by
/// construction). `PtrMetadata` is exact (`load p+8`, slice.sal layout) and
/// handled before the mnemonic lookup; everything else needs a mnemonic.
pub fn unop_needs_loud(op: &str) -> bool {
    if op == "PtrMetadata" {
        return false;
    }
    unop_mnemonic(op).is_none()
}

/// Bind a Move operand to a fresh temp (plain `=` already moves; `^` is
/// legal only in call args / store values); everything else renders inline.
/// Returns (preamble lines, operand text).
pub fn bind_move_operand(op: &Operand, bid: &str, idx: &mut usize) -> (Vec<String>, String) {
    // A projected operand never binds its root: loads/adds read the
    // base without consuming it (partial moves keep other fields live),
    // and the temp holds the field value itself.
    if operand_is_projected(op) {
        return field_value_temp(op, bid, idx);
    }
    match op {
        Operand::Move { place , .. } => {
            let tmp = format!("_mv_{}_{}", bid, idx);
            *idx += 1;
            (vec![format!("{} = {}", tmp, place)], tmp)
        }
        _ => (vec![], render_operand(op)),
    }
}

/// True when a call has args but no resolvable callee signature: no typed
/// `@extern` can be declared (bare `()` mismatches any args).
pub fn call_sig_loud(sig: &Option<CallSig>, args: &[Operand]) -> bool {
    sig.is_none() && !args.is_empty()
}

/// Build a reg->decimal-literal map by forward const propagation over the
/// given statements (emission/RPO order: defs precede dominated uses).
/// Seeds: `_X = <decimal>` (incl. `0` placeholders and scalar consts).
/// Propagates through `_X = _Y` (Move or Copy: re-materializing a literal
/// is always sound — literals carry no control or ownership dependence).
/// Invalidated by any other assignment to the reg. Rebind-loud skips keep
/// the old mapping (the skip keeps the first value, consistently).
/// Used to re-materialize literals at Copy/Move use sites instead of moving
/// a shared temp (which would trap UseAfterMove on the next use).
pub fn build_constmap<'a>(
    stmts: impl Iterator<Item = (&'a str, &'a crate::mir::Stmt)>,
) -> std::collections::HashMap<String, String> {
    use crate::mir::{Operand, Rvalue, Stmt};
    let mut map = std::collections::HashMap::new();
    for (_bid, st) in stmts {
        if let Stmt::Assign { dest, rvalue, .. } = st {
            match rvalue {
                Rvalue::Use { op } => match op {
                    Operand::Conflict { .. } => {
                        map.remove(dest);
                    }
                    Operand::Const { value, .. } => {
                        if let Some(d) = const_scalar_text(value) {
                            map.insert(dest.clone(), d);
                        } else if value.trim_start().starts_with("Val(ZeroSized") {
                            map.insert(dest.clone(), "0".to_string());
                        } else {
                            map.remove(dest);
                        }
                    }
                    Operand::Move { place , .. } | Operand::Copy { place , .. } => {
                        if let Some(v) = map.get(place).cloned() {
                            map.insert(dest.clone(), v);
                        } else {
                            map.remove(dest);
                        }
                    }
                    // An indexed value is not a literal: the dest's
                    // old literal mapping (if any) is invalidated.
                    Operand::Index { .. } => {
                        map.remove(dest);
                    }
                },
                _ => {
                    map.remove(dest);
                }
            }
        }
    }
    map
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use crate::mir::Operand;

    fn idx_op(
        place: &str,
        pty: Option<&str>,
        idx: Option<&str>,
        idx_size: u64,
        idx_const: Option<u64>,
        idx_fe: Option<u64>,
        deref: bool,
        deref_first: bool,
        fat: bool,
    ) -> Operand {
        Operand::Index {
            place: place.to_string(),
            off: 0,
            off2: 0,
            deref,
            deref_first,
            pty: pty.map(|t| t.to_string()),
            proj_loud: false,
            idx: idx.map(|t| t.to_string()),
            idx_size,
            idx_const,
            idx_fe,
            fat,
        }
    }

    #[test]
    fn dyn_index_value_loads_scaled_address() {
        let op = idx_op("_1", Some("u64"), Some("_2"), 8, None, None, false, false, false);
        assert!(operand_is_projected(&op));
        assert!(operand_is_field_projected(&op));
        assert!(!operand_proj_loud(&op));
        let mut c = 0;
        let lines = field_value_into(&op, "_0", "bb1", &mut c);
        let text = lines.join("\n");
        assert!(text.contains("mul"), "{}", text);
        assert!(text.contains("_2, 8"), "{}", text);
        assert!(text.contains("load"), "{}", text);
        assert!(lines.last().unwrap().starts_with("_0 = load"), "{}", text);
    }

    #[test]
    fn dyn_index_value_text() {
        let op = idx_op("_1", Some("u64"), Some("_2"), 8, None, None, false, false, false);
        let mut c = 0;
        let lines = field_value_into(&op, "_0", "bb1", &mut c);
        assert_eq!(
            lines,
            vec![
                "_mv_bb1_0 = mul _2, 8".to_string(),
                "_mv_bb1_1 = add _1, _mv_bb1_0".to_string(),
                "_0 = load _mv_bb1_1+0 as u64".to_string(),
            ]
        );
    }

    #[test]
    fn fat_index_value_loads_data_pointer_first() {
        let op = idx_op("_2", Some("u64"), Some("_4"), 8, None, None, true, true, true);
        let mut c = 0;
        let lines = field_value_into(&op, "_0", "bb1", &mut c);
        assert_eq!(lines[0], "_mv_bb1_0 = load _2+0 as ptr");
        assert!(lines.iter().any(|l| l.contains("mul _4, 8")));
        assert!(lines.last().unwrap().ends_with("as u64"));
    }

    #[test]
    fn const_fat_index_value_is_data_plus_bytes() {
        let op = idx_op("_2", Some("u64"), None, 8, Some(16), None, true, true, true);
        let mut c = 0;
        let lines = field_value_into(&op, "_0", "bb1", &mut c);
        // Const goes through the uniform address-temp path (the
        // temp + load 0 reads exactly the data+16 cell).
        assert_eq!(
            lines,
            vec![
                "_mv_bb1_0 = load _2+0 as ptr".to_string(),
                "_mv_bb1_1 = add _mv_bb1_0, 16".to_string(),
                "_0 = load _mv_bb1_1+0 as u64".to_string(),
            ]
        );
    }

    #[test]
    fn from_end_index_value_subtracts_from_length() {
        let op = idx_op("_2", Some("u64"), None, 8, None, Some(1), true, true, true);
        let mut c = 0;
        let lines = field_value_into(&op, "_0", "bb1", &mut c);
        let text = lines.join("\n");
        assert!(text.contains("load _2+8 as u64"), "{}", text);
        assert!(text.contains("sub"), "{}", text);
        assert!(text.contains("mul"), "{}", text);
    }

    #[test]
    fn index_addr_zero_offset_does_not_move_base() {
        let op = idx_op("_1", Some("u64"), None, 8, Some(0), None, false, false, false);
        let ipr = operand_index(&op).unwrap();
        let mut c = 0;
        let lines = index_addr_into(&ipr, "_d", "bb1", &mut c);
        assert_eq!(lines, vec!["_d = add _1, 0".to_string()]);
    }

    #[test]
    fn index_loud_gates() {
        // Aggregate-typed elements stay loud (two storage shapes).
        let op = idx_op("_1", Some("buf"), Some("_2"), 8, None, None, false, false, false);
        assert!(operand_proj_loud(&op));
        // No index form at all stays loud.
        let op = idx_op("_1", Some("u64"), None, 8, None, None, false, false, false);
        assert!(operand_proj_loud(&op));
        // Two index forms at once stay loud.
        let op = idx_op("_1", Some("u64"), Some("_2"), 8, Some(0), None, false, false, false);
        assert!(operand_proj_loud(&op));
        // Untyped stays loud.
        let op = idx_op("_1", None, Some("_2"), 8, None, None, false, false, false);
        assert!(operand_proj_loud(&op));
        // From-end without a fat length stays loud.
        let op = idx_op("_1", Some("u64"), None, 8, None, Some(1), false, false, false);
        assert!(operand_proj_loud(&op));
        // Fat without a deref stays loud.
        let op = idx_op("_1", Some("u64"), Some("_2"), 8, None, None, false, false, true);
        assert!(operand_proj_loud(&op));
    }

    #[test]
    fn zst_index_value_is_zero() {
        let op = idx_op("_1", Some("zst"), Some("_2"), 0, None, None, false, false, false);
        let mut c = 0;
        assert_eq!(field_value_into(&op, "_0", "bb1", &mut c), vec!["_0 = 0".to_string()]);
    }
}
