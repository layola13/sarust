//! mir2sa::mir.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// mir.json schema (same keys as the retired prototype so old JSON keeps working)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone)]
pub struct MirFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Crate name from sars (top-level "crate" key), when present.
    /// Absent for old mir.json and for `mir2sa parse` output.
    #[serde(default, rename = "crate", skip_serializing_if = "Option::is_none")]
    pub crate_name: Option<String>,
    pub functions: Vec<Function>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Function {
    pub name: String,
    /// Full def path (crate::mod::fn) from sars, when present.
    /// Absent for old mir.json and for mir2sa parse output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default)]
    pub locals: Vec<serde_json::Value>,
    /// MIR local types from sars (Debug text, e.g. "i32", "&i32",
    /// "Option<i32>"), keyed by base local ("_4"). Absent for old
    /// mir.json (serde default: empty); spill uses it to give scalar /
    /// pointer Copy sources a reload slot instead of an SA move.
    #[serde(default)]
    pub local_types: BTreeMap<String, String>,
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ret: Option<String>,
    #[serde(default = "default_sig_ok")]
    pub sig_ok: bool,
    pub blocks: Vec<Block>,
}

pub fn default_sig_ok() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Block {
    pub id: String,
    #[serde(default)]
    pub statements: Vec<Stmt>,
    #[serde(default = "default_return")]
    pub terminator: Term,
}

pub fn default_return() -> Term {
    Term::Return { ret: None }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum Stmt {
    Assign { dest: String, #[serde(default)] dest_place: Option<String>, rvalue: Rvalue },
    StorageLive { local: String },
    StorageDead { local: String },
    Nop { text: String },
    SetDisc { place: String, variant: u32, #[serde(default, skip_serializing_if = "Option::is_none")] place_full: Option<String> },
    UnsupportedStmt { text: String },
}

#[derive(Serialize, Deserialize, Clone)]
pub struct AdtLayout {
    pub size: u64,
    #[serde(default)]
    pub offsets: Vec<u64>,
}

/// Callee signature for typed `@extern` decls (bare `()` decls mismatch
/// any call with args: CapabilityMismatch). Absent when unresolvable.
#[derive(Serialize, Deserialize, Clone)]
pub struct CallSig {
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default = "default_sig_ret")]
    pub ret: String,
}

pub fn default_sig_ret() -> String {
    "i32".to_string()
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum Rvalue {
    Use { op: Operand },
    Ref {
        place: String,
        #[serde(rename = "mut", default, skip_serializing_if = "is_false")] mut_: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")] via: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")] zst: bool,
        /// Field-precision projection (driver place_proj): byte offset
        /// from the root buffer to the place (or to the deref cell when
        /// `deref` is set), trailing offset after the deref, deref
        /// shape, and the loud flag for unresolvable projections. All
        /// default to the old projectionless behaviour, so old mir.json
        /// (without these keys) parses exactly as before.
        #[serde(default)]
        off: u64,
        #[serde(default)]
        off2: u64,
        #[serde(default)]
        deref: bool,
        #[serde(default)]
        deref_first: bool,
        #[serde(default)]
        deref_last: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pty: Option<String>,
        #[serde(default)]
        proj_loud: bool,
        /// Index-ref (W1): one of idx / idx_const / idx_fe is set when
        /// the place's LAST projection step is an index. `idx` is the
        /// dynamic index local, `idx_size` its element stride;
        /// `idx_const` a static byte offset (fat ConstantIndex);
        /// `idx_fe` a slice from-end offset (effective index =
        /// fat-length - idx_fe); `fat` marks a fat-pointer deref in
        /// the chain (data pointer at pair +0, length at +8).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idx: Option<String>,
        #[serde(default)]
        idx_size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idx_const: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idx_fe: Option<u64>,
        #[serde(default)]
        fat: bool,
    },
    Call {
        func: String,
        #[serde(default)] args: Vec<Operand>,
        #[serde(default, skip_serializing_if = "Option::is_none")] sig: Option<CallSig>,
    },
    BinOp { op: String, left: Box<Operand>, right: Box<Operand> },
    UnOp { op: String, operand: Box<Operand> },
    Cast {
        op: Box<Operand>,
        ty: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        castkind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        src_ty: Option<String>,
    },
    Aggregate {
        elems: Vec<Operand>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layout: Option<AdtLayout>,
    },
    Discriminant { place: String },
    RawPtr {
        place: String,
        #[serde(rename = "mut", default, skip_serializing_if = "is_false")] mut_: bool,
        #[serde(default)]
        off: u64,
        #[serde(default)]
        off2: u64,
        #[serde(default)]
        deref: bool,
        #[serde(default)]
        deref_first: bool,
        #[serde(default)]
        deref_last: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pty: Option<String>,
        #[serde(default)]
        proj_loud: bool,
        /// Index-ref (W1): one of idx / idx_const / idx_fe is set when
        /// the place's LAST projection step is an index. `idx` is the
        /// dynamic index local, `idx_size` its element stride;
        /// `idx_const` a static byte offset (fat ConstantIndex);
        /// `idx_fe` a slice from-end offset (effective index =
        /// fat-length - idx_fe); `fat` marks a fat-pointer deref in
        /// the chain (data pointer at pair +0, length at +8).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idx: Option<String>,
        #[serde(default)]
        idx_size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idx_const: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idx_fe: Option<u64>,
        #[serde(default)]
        fat: bool,
    },
    Repeat { op: Box<Operand>, len: String },
    ThreadLocal { def: String },
    Unsupported { text: String },
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum Operand {
    Move {
        place: String,
        #[serde(default)]
        off: u64,
        #[serde(default)]
        off2: u64,
        #[serde(default)]
        deref: bool,
        #[serde(default)]
        deref_first: bool,
        #[serde(default)]
        deref_last: bool,
        /// SA kind of the projected place: scalar name, "ptr" for a
        /// reference/raw-pointer/fn-pointer value, "buf" for an
        /// aggregate/array/tuple value (a pointer slot in mir2sa
        /// buffers), "zst" for a zero-sized value. None for old mir.json
        /// and for projectionless places.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pty: Option<String>,
        #[serde(default)]
        proj_loud: bool,
    },
    Copy {
        place: String,
        #[serde(default)]
        off: u64,
        #[serde(default)]
        off2: u64,
        #[serde(default)]
        deref: bool,
        #[serde(default)]
        deref_first: bool,
        #[serde(default)]
        deref_last: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pty: Option<String>,
        #[serde(default)]
        proj_loud: bool,
    },
    /// Indexed read (W1 index-ref): the place's LAST projection step
    /// is an index into an array/slice. Static field/deref fields
    /// mean exactly what they mean for Copy/Move; the index itself is
    /// one of `idx` (dynamic index local, stride `idx_size`),
    /// `idx_const` (static byte offset, fat ConstantIndex), or
    /// `idx_fe` (slice from-end offset, effective index = fat-length
    /// - idx_fe, stride `idx_size`). `fat` marks a fat-pointer deref
    /// in the chain. A second projection step after the index never
    /// produces this variant (the driver stays loud there), so
    /// `deref_last` has no meaning here and is omitted.
    Index {
        place: String,
        #[serde(default)]
        off: u64,
        #[serde(default)]
        off2: u64,
        #[serde(default)]
        deref: bool,
        #[serde(default)]
        deref_first: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pty: Option<String>,
        #[serde(default)]
        proj_loud: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idx: Option<String>,
        #[serde(default)]
        idx_size: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idx_const: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idx_fe: Option<u64>,
        #[serde(default)]
        fat: bool,
    },
    Const {
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        str_bytes: Option<Vec<u64>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        str_len: Option<u64>,
    },
    /// Join-ambiguous use inserted by `version.rs`: two or more reaching
    /// definitions with no dominance order (needs phi nodes, which SA lacks).
    /// The original operand's projection rides along (off/deref/pty):
    /// phi.rs resolves a projected field read through a slot instead of
    /// dropping the field, and unresolvable Conflicts still go loud via
    /// `const_needs_loud`, counted identically by lower and coverage.
    /// New fields default so old mir.json (projectionless) parses as before.
    Conflict {
        place: String,
        #[serde(default)]
        off: u64,
        #[serde(default)]
        off2: u64,
        #[serde(default)]
        deref: bool,
        #[serde(default)]
        deref_first: bool,
        #[serde(default)]
        deref_last: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pty: Option<String>,
        #[serde(default)]
        proj_loud: bool,
    },
}

/// Max string-literal bytes materialized inline per `&str` field (byte-wise
/// `store`s; longer literals stay loud UNSUPPORTED).
pub const STR_INLINE_MAX: u64 = 64;

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum Term {
    Goto { target: String },
    /// `ret` is the resolved return register (MIR returns through local `_0`,
    /// renamed by `version.rs`); `None` when unresolved and
    /// `__VERSION_CONFLICT__` when a join gives two reaching versions.
    Return {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ret: Option<String>,
    },
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sig: Option<CallSig>,
    },
    Drop { place: String, target: String },
    SwitchInt { discr: Box<Operand>, #[serde(default)] targets: Vec<(String, String)>, otherwise: String },
    Assert { cond: Box<Operand>, target: String, #[serde(default, skip_serializing_if = "Option::is_none")] msg: Option<String>, #[serde(default, skip_serializing_if = "Option::is_none")] expected: Option<bool> },
    Unreachable,
    InlineAsm {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        template: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        options: Option<String>,
        #[serde(default)]
        modifiers: bool,
        #[serde(default)]
        inout: bool,
        #[serde(default)]
        outs: Vec<String>,
        #[serde(default)]
        ins: Vec<Operand>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
    Unsupported { text: String },
}

pub fn is_false(b: &bool) -> bool {
    !b
}
