//! rsc_driver::place.
use crate::util::{local_name, trunc};
use rustc_middle::mir::Place;

pub fn place_name(p: &Place<'_>) -> (String, String) {
    let full = trunc(format!("{:?}", p), 80);
    match p.as_local() {
        Some(l) => (local_name(l), full),
        None => {
            // Base-local collapse (documented approximation): every MIR
            // projection reads through its root local. This replaced the old
            // Debug-scraping heuristics (leading `_N`, pure-deref) which
            // missed shapes like double-parenthesized `((_1.0: …))` and fell
            // back to the `_proj` placeholder — an undefined register that
            // can never assemble. Full text stays in `place_via`/comments.
            (local_name(p.local), full)
        }
    }
}

/// Original place text when place_name had to approximate (deref chains,

pub fn place_via(p: &Place<'_>) -> Option<String> {
    match p.as_local() {
        Some(_) => None,
        None => Some(trunc(format!("{:?}", p), 80)),
    }
}
