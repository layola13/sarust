//! mir2sa::mir.
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// mir.json schema (same keys as the retired prototype so old JSON keeps working)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
pub struct MirFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub functions: Vec<Function>,
}

#[derive(Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    #[serde(default)]
    pub locals: Vec<serde_json::Value>,
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

#[derive(Serialize, Deserialize)]
pub struct Block {
    pub id: String,
    #[serde(default)]
    pub statements: Vec<Stmt>,
    #[serde(default = "default_return")]
    pub terminator: Term,
}

pub fn default_return() -> Term {
    Term::Return
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Stmt {
    Assign { dest: String, #[serde(default)] dest_place: Option<String>, rvalue: Rvalue },
    StorageLive { local: String },
    StorageDead { local: String },
    Nop { text: String },
    SetDisc { place: String, variant: u32, #[serde(default, skip_serializing_if = "Option::is_none")] place_full: Option<String> },
    UnsupportedStmt { text: String },
}

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Rvalue {
    Use { op: Operand },
    Ref {
        place: String,
        #[serde(rename = "mut", default, skip_serializing_if = "is_false")] mut_: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")] via: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")] zst: bool,
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
    RawPtr { place: String, #[serde(rename = "mut", default, skip_serializing_if = "is_false")] mut_: bool },
    Repeat { op: Box<Operand>, len: String },
    ThreadLocal { def: String },
    Unsupported { text: String },
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum Operand {
    Move { place: String },
    Copy { place: String },
    Const {
        value: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        str_bytes: Option<Vec<u64>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        str_len: Option<u64>,
    },
}

/// Max string-literal bytes materialized inline per `&str` field (byte-wise
/// `store`s; longer literals stay loud UNSUPPORTED).
pub const STR_INLINE_MAX: u64 = 64;

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Term {
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
