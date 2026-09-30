//! rsc_driver::util.
use rustc_middle::mir::{BasicBlock, Local};
use std::fmt::Write as _;

pub fn esc(s: &str, into: &mut String) {
    for c in s.chars() {
        match c {
            '"' => into.push_str("\\\""),
            '\\' => into.push_str("\\\\"),
            '\n' => into.push_str("\\n"),
            '\r' => into.push_str("\\r"),
            '\t' => into.push_str("\\t"),
            c if (c as u32) < 0x20 => write!(into, "\\u{:04x}", c as u32).unwrap(),
            c => into.push(c),
        }
    }
}

pub fn trunc(s: String, n: usize) -> String {
    if s.len() > n { s[..n].to_string() } else { s }
}

pub fn local_name(l: Local) -> String {
    format!("_{}", l.as_u32())
}

/// (short, full): short is the base local when projectionless, else the
/// place's root local (`p.local` — exact, no Debug scraping).
/// Pure deref chains `(*_N)` resolve to `_N` (reborrow keeps the borrow

pub fn sanitize(raw: &str) -> String {
    let mut s = String::new();
    let mut sep = true;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            s.push(ch);
            sep = false;
        } else if !sep {
            s.push('_');
            sep = true;
        }
    }
    while s.ends_with('_') {
        s.pop();
    }
    s.chars().take(80).collect()
}

pub fn bb_name(b: BasicBlock) -> String {
    format!("bb{}", b.index())
}
