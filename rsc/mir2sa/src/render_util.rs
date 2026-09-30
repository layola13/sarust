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
        Operand::Conflict { place } => {
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
        Operand::Const { value, str_bytes, str_len } => {
            let inlineable = crate::layout::plan_const_bytes(
                "_probe", "bb", 0, value, str_bytes.as_ref(), *str_len,
            )
            .is_some();
            !inlineable && const_needs_loud(op)
        }
        _ => const_needs_loud(op),
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
        Rvalue::Call { args, .. } => args.iter().find(|a| const_needs_loud(a)),
        Rvalue::Aggregate { elems, .. } if elems.len() <= 1 => {
            elems.first().filter(|e| matches!(e, Operand::Const { .. }) && const_needs_loud(e))
        }
        _ => None,
    };
    hit.map(|op| match op {
        Operand::Conflict { place } => {
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
        Operand::Move { place } => place.clone(),
        Operand::Copy { place } => place.clone(),
        // Conflict sentinels never reach here (intercepted loud upstream);
        // the fallback keeps rendering total.
        Operand::Conflict { place } => place.clone(),
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
        Operand::Move { place } => place.clone(),
        _ => render_operand(op),
    }
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
    match op {
        Operand::Move { place } => {
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
                    Operand::Move { place } | Operand::Copy { place } => {
                        if let Some(v) = map.get(place).cloned() {
                            map.insert(dest.clone(), v);
                        } else {
                            map.remove(dest);
                        }
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
