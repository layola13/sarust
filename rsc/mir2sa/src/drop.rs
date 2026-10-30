//! mir2sa::drop — exit-anchored release insertion (Drop glue, phase 1).
//!
//! Goal: eliminate MemoryLeak traps WITHOUT introducing new traps.
//! Conservative rules (all verified against `sa check` probes):
//! - `x = y` MOVES (source dead after); call/store/eq/load/br read shared.
//! - `&y` locks y (freeing y while the borrow is live -> BorrowConflict).
//! - `!r` frees; double-free and use-after-free trap.
//! - regs are function-global single-assignment (rebinds already loud).
//!
//! Algorithm per function:
//! 1. Scan body lines: defs (`r = ...`), borrows (`x = &y`), frees (`!r`),
//!    move-consumed sources (RHS plain reg of `=`).
//! 2. For each Return-terminated block B, candidates = regs that are
//!    defined exactly once in a dominator of B (or params), never
//!    move-consumed, never explicitly freed on a dominator path of B.
//! 3. Order candidates borrows-before-sources over the borrow graph;
//!    skip anything ambiguous (cycles, unknowns).
//! 4. Caller inserts `!r` lines before `return`.
//!
//! Non-goals (separate programs): UseAfterMove (needs re-borrow/reload),
//! BorrowConflict repair (needs borrow-end analysis), PhiStateConflict
//! (needs path-sensitive cleanup), non-Return exits.

use std::collections::{BTreeMap, BTreeSet};

/// One body line classified for liveness. Plain-text, no MIR needed
/// (operates on the already-lowered SA lines).
#[derive(Debug, Default)]
struct LineUse {
    def: Option<String>,
    /// Plain-reg RHS sources of `=` (move-consumers).
    move_srcs: Vec<String>,
    /// Borrow edge (borrower, source) for `x = &y`.
    borrow: Option<(String, String)>,
    freed: Vec<String>,
}

fn is_reg(tok: &str) -> bool {
    let t = tok.trim().trim_end_matches(',');
    (t.starts_with('_') || t.chars().next().is_some_and(|c| c.is_ascii_alphabetic()))
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !t.is_empty()
        && !matches!(
            t,
            "call"
                | "return"
                | "jmp"
                | "br"
                | "eq"
                | "ne"
                | "add"
                | "sub"
                | "mul"
                | "and"
                | "or"
                | "xor"
                | "shl"
                | "not"
                | "neg"
                | "sext"
                | "zext"
                | "trunc"
                | "bitcast"
                | "fptosi"
                | "sitofp"
                | "uitofp"
                | "fpext"
                | "fptrunc"
                | "load"
                | "store"
                | "alloc"
                | "panic"
                | "unreachable"
                | "as"
        )
}

fn classify(line: &str) -> LineUse {
    let mut u = LineUse::default();
    let s = line.trim();
    if s.is_empty() || s.starts_with("//") || s.ends_with(':') {
        return u;
    }
    if let Some(rest) = s.strip_prefix('!') {
        let r = rest.trim().split_whitespace().next().unwrap_or("");
        if is_reg(r) {
            u.freed.push(r.to_string());
        }
        return u;
    }
    // `return <reg>` CONSUMES the reg (probe r4): it must not also be a
    // free candidate, or the exit free double-releases it.
    if let Some(rest) = s.strip_prefix("return ") {
        let r = rest.trim();
        if is_reg(r) {
            u.move_srcs.push(r.to_string());
        }
        return u;
    }
    if let Some(eq) = s.find(" = ") {
        let dest = s[..eq].trim();
        if is_reg(dest) {
            u.def = Some(dest.to_string());
        }
        let rhs = s[eq + 3..].trim();
        // Borrow shape: `x = &y` (y may carry `//`? no — trailing comments
        // are forbidden, so the line is clean).
        if let Some(src) = rhs.strip_prefix('&') {
            let src = src.trim().split_whitespace().next().unwrap_or("");
            if is_reg(src) {
                if let Some(d) = &u.def {
                    u.borrow = Some((d.clone(), src.to_string()));
                }
                return u;
            }
        }
        // Plain move: RHS is exactly one reg (not a call/compound).
        if is_reg(rhs) && !rhs.contains('(') && !rhs.contains(' ') {
            u.move_srcs.push(rhs.to_string());
        }
        return u;
    }
    u
}

/// Register-like tokens mentioned on a line.
fn regs_in(line: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for tok in line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
        let t = tok.trim();
        if t.is_empty() || t.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        if (t.starts_with('_') || t.chars().next().is_some_and(|c| c.is_ascii_alphabetic()))
            && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            out.insert(t.to_string());
        }
    }
    out
}

/// Per-block frees for the backend's OWN comparison temps (`_as_*`, `_sw_*`).
///
/// These are synthetic: an `Assert`/`SwitchInt` binds a cond temp, compares it,
/// and branches. Their liveness is provably block-local — nothing after the
/// block reads them — so they are released at the block's own end instead of
/// waiting for an exit, where the dominance gate skips them (a `SwitchInt` temp
/// lives in a block that need not dominate the exit, which is why
/// `129_seqlock_optimistic` and friends leaked exactly one `_as_*`/`_sw_*`).
///
/// Deliberately narrow: value registers are NOT touched. T24a measured that
/// freeing those on a branch trades ~5 leaks for ~10 UseAfterMove, because a
/// text-level "dead after the join" gate cannot see every path. Comparison
/// temps have no such reads by construction.
pub fn temp_frees(body_lines: &[Vec<String>]) -> BTreeMap<usize, Vec<String>> {
    // Where each compare temp is mentioned, per block.
    let mut where_: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        for l in ls {
            for r in regs_in(l.as_str()) {
                if is_compare_temp(&r) {
                    where_.entry(r).or_default().insert(bi);
                }
            }
        }
    }
    let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        let mut defined: Vec<String> = vec![];
        let mut freed: BTreeSet<String> = BTreeSet::new();
        let mut moved: BTreeSet<String> = BTreeSet::new();
        for l in ls {
            let u = classify(l);
            if let Some(d) = &u.def {
                if is_compare_temp(d) {
                    defined.push(d.clone());
                }
            }
            for m in &u.move_srcs {
                moved.insert(m.clone());
            }
            for f in &u.freed {
                freed.insert(f.clone());
            }
        }
        // A compare temp is READ by its own `eq`/`br` — that is its intended
        // use, not liveness. What matters is that no OTHER block mentions it
        // (then it is dead once this block ends) and that it was never moved
        // (a compare temp is never the RHS of a plain assign).
        let keep: Vec<String> = defined
            .into_iter()
            .filter(|r| !freed.contains(r))
            .filter(|r| where_.get(r).is_some_and(|bs| bs.len() == 1 && bs.contains(&bi)))
            // A temp consumed by a BRANCH must outlive the block: in an
            // `Assert` block the `br cond -> ..` sits mid-block (the fail arm
            // follows as a label), so "not on the last line" is not enough.
            // Nothing may follow the terminator, so such temps are skipped.
            .filter(|r| {
                !ls.iter().any(|l| {
                    let t = l.trim_start();
                    (t.starts_with("br ") || t.starts_with("return "))
                        && regs_in(l.as_str()).contains(r)
                })
            })
            .collect();
        if !keep.is_empty() {
            out.insert(bi, keep);
        }
    }
    out
}

/// Block-local release for MIR Move-operand temps (`_mv_*`).
///
/// Shape (182_mmap_memory_mapping, 129_seqlock_optimistic): a branch block
/// binds `_mv_bbN_K = <local>` and consumes it in one statement of the same
/// block (`_v = eq _mv_bbN_K, ...`), then joins a bypass block. The exit
/// block is dominated by neither branch, so `exit_frees` (dominance-gated)
/// never releases the temp and `sa check` traps MemoryLeak at `return`.
/// Releasing at the exit is unsound (UnknownRegister on the bypass path,
/// the `_constant_0_` incident); extending `temp_frees` to `_mv_` broke
/// UseAfterMove when the temp was consumed by the terminator. So the release
/// goes immediately after the temp LAST use inside its own block, and only
/// when every guard holds:
/// - defined exactly once, in block B;
/// - every non-comment mention is in B (temps are single-statement bindings
///   by construction; a cross-block mention vetoes);
/// - never move-consumed (`x = _mv_...`), never borrowed-from
///   (`x = &_mv_...`), never already freed (`!_mv_...`) anywhere;
/// - every non-def mention is a read on a computation line (`dest = <op> ...`
///   with a read mnemonic); call/store/branch/return mentions veto, since
///   their ownership shape is not a plain read;
/// - the last use follows the def, is not the block last line (a
///   terminator must not be followed by a label-less line), and no mid-block
///   label sits between the def and the last use (same straight-line region).
/// Returns per-block (insert-after line index, reg) pairs, sorted by line
/// then reg. Callers must still exclude regs already released by
/// `exit_frees` / `temp_frees` (double-free traps); this function does not
/// know those lists.
pub fn mv_frees(body_lines: &[Vec<String>]) -> BTreeMap<usize, Vec<(usize, String)>> {
    const READ_OPS: [&str; 23] = [
        "eq", "ne", "and", "or", "xor", "not", "neg", "sext", "zext", "trunc", "bitcast", "load",
        "fptosi", "sitofp", "uitofp", "fpext", "fptrunc", "add", "sub", "mul", "div", "rem", "shl",
    ];
    let is_mv = |r: &str| r.starts_with("_mv_");
    let is_label = |l: &str| {
        let t = l.trim();
        t.starts_with("L_") && t.ends_with(':') && !t.contains(' ')
    };
    let mut def_count: BTreeMap<String, usize> = BTreeMap::new();
    let mut def_at: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut mentions: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    let mut moved: BTreeSet<String> = BTreeSet::new();
    let mut borrowed_from: BTreeSet<String> = BTreeSet::new();
    let mut freed: BTreeSet<String> = BTreeSet::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        for (li, l) in ls.iter().enumerate() {
            if l.trim_start().starts_with("//") {
                continue;
            }
            let u = classify(l);
            if let Some(d) = &u.def {
                if is_mv(d) {
                    *def_count.entry(d.clone()).or_insert(0) += 1;
                    def_at.entry(d.clone()).or_insert((bi, li));
                }
            }
            for m in &u.move_srcs {
                if is_mv(m) {
                    moved.insert(m.clone());
                }
            }
            for f in &u.freed {
                if is_mv(f) {
                    freed.insert(f.clone());
                }
            }
            if let Some((_, s)) = &u.borrow {
                if is_mv(s) {
                    borrowed_from.insert(s.clone());
                }
            }
            for r in regs_in(l.as_str()) {
                if is_mv(&r) {
                    mentions.entry(r).or_default().push((bi, li));
                }
            }
        }
    }
    let mut out: BTreeMap<usize, Vec<(usize, String)>> = BTreeMap::new();
    for (r, &(dbi, dli)) in def_at.iter() {
        if def_count.get(r).is_some_and(|&n| n != 1) {
            continue;
        }
        if moved.contains(r) || borrowed_from.contains(r) || freed.contains(r) {
            continue;
        }
        let ms = match mentions.get(r) {
            Some(ms) => ms,
            None => continue,
        };
        if ms.iter().any(|(b, _)| *b != dbi) {
            continue;
        }
        let uses: Vec<usize> = ms
            .iter()
            .map(|(_, li)| *li)
            .filter(|&li| li != dli)
            .collect();
        if uses.is_empty() {
            continue;
        }
        let last = *uses.iter().max().unwrap();
        if last <= dli {
            continue;
        }
        let block = &body_lines[dbi];
        if last + 1 >= block.len() {
            continue;
        }
        if (dli..=last).any(|k| block.get(k).is_some_and(|l| is_label(l))) {
            continue;
        }
        let read_ok = uses.iter().all(|&li| {
            let line = block.get(li).map(|l| l.trim()).unwrap_or("");
            match line.find(" = ") {
                Some(eq) => {
                    let rhs = line[eq + 3..].trim();
                    let op = rhs.split_whitespace().next().unwrap_or("");
                    READ_OPS.contains(&op)
                }
                None => false,
            }
        });
        if !read_ok {
            continue;
        }
        out.entry(dbi).or_default().push((last, r.clone()));
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

/// The backend's own synthetic, block-local temporaries:
/// `_as_*` (assert cond) and `_sw_*` (switch-int cond) plus their `_eq*`
/// compare temps. `_mv_*` is deliberately EXCLUDED here: extending this
/// class to it was measured to introduce UseAfterMove. Move temps are
/// handled instead by `move_frees` below, under strictly narrower gates
/// (single definition, single-block mention, straight-line block, never
/// moved, never a borrow source) that the compare-temp gates do not give.
fn is_compare_temp(reg: &str) -> bool {
    reg.starts_with("_as_") || reg.starts_with("_sw_")
}

/// Per-block frees for plain-move dests (`r = s`, s a register) that leak
/// for the same dominance reason as compare temps but are NOT compare
/// temps: the move transfers ownership to `r`, and if the defining block
/// does not dominate the exit, `exit_frees` skips `r` (129:
/// `_mv_bb11_*` in bb11 and the versioned `_32_v0` in bb15, both plain
/// moves in non-dominating blocks; operation results such as `_27 = and
/// ...` and `_21_v0 = eq ...` do NOT leak, verified by sa-check probes
/// on the real 129 output, so they are not in this class).
///
/// Gates (all must hold; anything else is left to `exit_frees`, i.e.
/// under-freeing, never a speculative free):
/// - the block is straight-line: no mid-block label (its first line is
///   its own label) and its last line is a terminator (`jmp`/`br`/
///   `return`/`panic`). The free is then inserted before the terminator,
///   after every use in the block, on the block's sole path.
/// - `r` is defined exactly once in the whole function, by that plain
///   move, and mentioned in no other block (comment lines do not count
///   as mentions: loud comments name dests in other blocks).
/// - `r` is never freed anywhere, never plain-moved (a move would
///   already consume it), never read by a `br`/`return` line in its
///   block, and never a borrow source (its liveness could extend
///   through a borrower past the block).
/// - `_as_*`/`_sw_*` temps are excluded: they are `temp_frees`' domain
///   (their base temps ARE plain moves, e.g. `_as_bb9 = _24`); the two
///   classes must stay disjoint so a register is never freed twice.
pub fn move_frees(body_lines: &[Vec<String>]) -> BTreeMap<usize, Vec<String>> {
    let is_label = |l: &str| {
        let t = l.trim();
        t.starts_with("L_") && t.ends_with(':') && !t.contains(' ')
    };
    let is_comment = |l: &str| l.trim_start().starts_with("//");
    let cls: Vec<Vec<LineUse>> =
        body_lines.iter().map(|ls| ls.iter().map(|l| classify(l)).collect()).collect();
    let mut def_count: BTreeMap<String, usize> = BTreeMap::new();
    let mut moved: BTreeSet<String> = BTreeSet::new();
    let mut freed_anywhere: BTreeSet<String> = BTreeSet::new();
    let mut borrow_sources: BTreeSet<String> = BTreeSet::new();
    let mut where_: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        for u in cls[bi].iter() {
            if let Some(d) = &u.def {
                *def_count.entry(d.clone()).or_insert(0) += 1;
            }
            for m in &u.move_srcs {
                moved.insert(m.clone());
            }
            for f in &u.freed {
                freed_anywhere.insert(f.clone());
            }
            if let Some((_, src)) = &u.borrow {
                borrow_sources.insert(src.clone());
            }
        }
        for l in ls {
            if is_comment(l) {
                continue;
            }
            for r in regs_in(l.as_str()) {
                where_.entry(r).or_default().insert(bi);
            }
        }
    }
    let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        if ls.is_empty() {
            continue;
        }
        // Straight-line only: no label past the block's own first line.
        if ls.iter().skip(1).any(|l| is_label(l)) {
            continue;
        }
        // Last line must be a terminator; otherwise the insertion point
        // (before the last line) is not provably after every use.
        let last = ls.last().map(|l| l.trim_start()).unwrap_or("");
        if !(last.starts_with("jmp ")
            || last.starts_with("br ")
            || last.starts_with("return")
            || last.starts_with("panic("))
        {
            continue;
        }
        let br_return_reads: BTreeSet<String> = ls
            .iter()
            .filter(|l| {
                let t = l.trim_start();
                t.starts_with("br ") || t.starts_with("return ")
            })
            .flat_map(|l| regs_in(l.as_str()))
            .collect();
        let mut keep: Vec<String> = vec![];
        for u in cls[bi].iter() {
            let Some(d) = &u.def else { continue };
            // Plain move only: exactly one move source, not a borrow.
            if u.move_srcs.len() != 1 || u.borrow.is_some() {
                continue;
            }
            if is_compare_temp(d) {
                continue;
            }
            if def_count.get(d).is_some_and(|&n| n != 1) {
                continue;
            }
            if moved.contains(d) || freed_anywhere.contains(d) || borrow_sources.contains(d) {
                continue;
            }
            if !where_.get(d).is_some_and(|bs| bs.len() == 1 && bs.contains(&bi)) {
                continue;
            }
            if br_return_reads.contains(d) {
                continue;
            }
            keep.push(d.clone());
        }
        if !keep.is_empty() {
            out.insert(bi, keep);
        }
    }
    out
}

/// Per-block frees for CALL dests consumed exactly once as a call argument
/// in a later (or same) block (`_3 = call @from_str(...)` in bb1, then
/// `call @print(_3)` in bb2, then nothing: `_3` is dead after bb2's call).
///
/// Why this class exists: a call dest defined in a block that does not
/// dominate the exit is skipped by `exit_frees` (dominance gate), and it
/// is not a plain move, so `move_frees` skips it too. When the call that
/// defined it was loud, the dest was a `0` placeholder (not an
/// allocation) and nothing leaked; un-louding the call turns the dest
/// into a real allocation and the latent gap becomes a MemoryLeak trap
/// at `return` (105: `_3`, 56: `_8` — both `from_str` results printed
/// once on a branch path). The gap is pre-existing and was masked by
/// loud placeholders, exactly like the binop/cast louds that un-louding
/// a const exposes; this rule closes it for the narrow shape below.
///
/// Gates (all must hold; anything else is left to `exit_frees`, i.e.
/// under-freeing, never a speculative free):
/// - the register is defined exactly once in the whole function, by a
///   call-dest line (`r = call @f(...)`, Term::Call or Rvalue::Call);
/// - it is mentioned in exactly two non-comment lines total: the def
///   line and one call-argument use line (`call @g(..., r, ...)` or
///   `d = call @g(..., r, ...)`), with the use line after the def line
///   (same block: later line; other block: any line — a use in an
///   earlier block than the def would make the use itself trap first,
///   and the free sits after the use in the use block, so it cannot
///   introduce a new trap class);
/// - it is never freed anywhere, never a borrow source, never a plain
///   move source (every such event would be a third mention line and
///   already vetoes via the two-line gate; the checks are explicit so a
///   future classify change cannot silently widen this);
/// - the use block is straight-line (no label past its first line) and
///   ends in a terminator (`jmp`/`br`/`return`/`panic`), so the
///   pre-terminator insertion point (shared with `tfrees`) is provably
///   after the consuming call;
/// - no join mixes a freed path with an un-freed defined path: the set
///   of blocks reachable from the def block without passing through the
///   use block must be disjoint from the set reachable from the use
///   block. Otherwise the join would see Freed on one incoming edge and
///   Active on another and `sa check` traps PhiStateConflict (129,
///   185: def in a dominating block, use on one branch only). A veto
///   here simply keeps baseline behavior (no free), never a new trap.
///   The 105 shape passes this gate: the def block's only successor is
///   the use block, so the without-use set is just the def block itself.
/// A spilled dest is vetoed automatically: its slot store is a third
/// mention line. The caller merges this into `tfrees`, so the existing
/// dedup and one-direction cross-filter against `exit_frees` apply
/// (a register freed here is never also freed at an exit).
pub fn call_dest_frees(body_lines: &[Vec<String>]) -> BTreeMap<usize, Vec<String>> {
    let is_label = |l: &str| {
        let t = l.trim();
        t.starts_with("L_") && t.ends_with(':') && !t.contains(' ')
    };
    let is_comment = |l: &str| l.trim_start().starts_with("//");
    // Args of a call line (`call @f(a, b)` or `d = call @f(a, b)`),
    // or None when the line is not a call.
    let call_args = |line: &str| -> Option<Vec<String>> {
        let t = line.trim_start();
        let rest = t.strip_prefix("call @").or_else(|| {
            let eq = t.find(" = call @")?;
            Some(&t[eq + " = call @".len()..])
        })?;
        let open = rest.find('(')?;
        let mut depth = 0usize;
        let mut end = None;
        for (i, ch) in rest[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let args = &rest[open + 1..end?];
        if args.trim().is_empty() {
            return Some(vec![]);
        }
        Some(
            args.split(',')
                .map(|a| a.trim().trim_start_matches('^').to_string())
                .collect(),
        )
    };
    let is_call_dest = |line: &str| -> Option<String> {
        let t = line.trim_start();
        let eq = t.find(" = call @")?;
        let dest = t[..eq].trim();
        if is_reg(dest) {
            Some(dest.to_string())
        } else {
            None
        }
    };
    let cls: Vec<Vec<LineUse>> =
        body_lines.iter().map(|ls| ls.iter().map(|l| classify(l)).collect()).collect();
    let mut def_count: BTreeMap<String, usize> = BTreeMap::new();
    let mut def_pos: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut freed_anywhere: BTreeSet<String> = BTreeSet::new();
    let mut moved: BTreeSet<String> = BTreeSet::new();
    let mut borrow_sources: BTreeSet<String> = BTreeSet::new();
    // (block, line) of every non-comment mention, per register.
    let mut mentions: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        for (li, l) in ls.iter().enumerate() {
            if is_comment(l) {
                continue;
            }
            let u = &cls[bi][li];
            if let Some(d) = &u.def {
                *def_count.entry(d.clone()).or_insert(0) += 1;
                def_pos.entry(d.clone()).or_insert((bi, li));
            }
            for f in &u.freed {
                freed_anywhere.insert(f.clone());
            }
            for m in &u.move_srcs {
                moved.insert(m.clone());
            }
            if let Some((_, src)) = &u.borrow {
                borrow_sources.insert(src.clone());
            }
            for r in regs_in(l.as_str()) {
                mentions.entry(r).or_default().push((bi, li));
            }
        }
    }
    // Successor graph over first-line labels only (mid-block switch /
    // assert labels stay intra-block, as in `stale_out_sets`).
    let mut label_block: BTreeMap<&str, usize> = BTreeMap::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        if let Some(first) = ls.first() {
            let t = first.trim();
            if t.starts_with("L_") && t.ends_with(':') && !t.contains(' ') {
                label_block.insert(t.trim_end_matches(':'), bi);
            }
        }
    }
    let mut succ: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); body_lines.len()];
    for (bi, ls) in body_lines.iter().enumerate() {
        for l in ls {
            let t = l.trim_start();
            if let Some(rest) = t.strip_prefix("jmp ") {
                if let Some(&tb) = label_block.get(rest.trim()) {
                    succ[bi].insert(tb);
                }
            } else if let Some(rest) = t.strip_prefix("br ") {
                if let Some(arrow) = rest.find("->") {
                    for tgt in rest[arrow + 2..].split(',') {
                        if let Some(&tb) = label_block.get(tgt.trim()) {
                            succ[bi].insert(tb);
                        }
                    }
                }
            }
        }
    }
    let reachable = |from: usize, avoid: Option<usize>| -> BTreeSet<usize> {
        let mut seen: BTreeSet<usize> = BTreeSet::new();
        let mut stack = vec![from];
        while let Some(cur) = stack.pop() {
            if Some(cur) == avoid {
                continue;
            }
            if !seen.insert(cur) {
                continue;
            }
            for &n in &succ[cur] {
                stack.push(n);
            }
        }
        seen
    };
    let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (r, ms) in &mentions {
        if def_count.get(r).copied().unwrap_or(0) != 1 {
            continue;
        }
        let (dbi, dli) = def_pos[r];
        if is_call_dest(&body_lines[dbi][dli]).as_deref() != Some(r.as_str()) {
            continue;
        }
        if ms.len() != 2 {
            continue;
        }
        let use_pos = ms.iter().find(|&&(bi, li)| !(bi == dbi && li == dli));
        let Some(&(ubi, uli)) = use_pos else { continue };
        if ubi == dbi && uli <= dli {
            continue;
        }
        if freed_anywhere.contains(r) || moved.contains(r) || borrow_sources.contains(r) {
            continue;
        }
        let use_line = &body_lines[ubi][uli];
        let Some(args) = call_args(use_line) else { continue };
        if !args.iter().any(|a| a == r) {
            continue;
        }
        // Join gate: no block may be reachable both without the use
        // block (from the def) and from the use block.
        let without_use = reachable(dbi, Some(ubi));
        let from_use = reachable(ubi, None);
        if !without_use.is_disjoint(&from_use) {
            continue;
        }
        // Straight-line use block ending in a terminator.
        let uls = &body_lines[ubi];
        if uls.iter().skip(1).any(|l| is_label(l)) {
            continue;
        }
        let last = uls.last().map(|l| l.trim_start()).unwrap_or("");
        if !(last.starts_with("jmp ")
            || last.starts_with("br ")
            || last.starts_with("return")
            || last.starts_with("panic("))
        {
            continue;
        }
        out.entry(ubi).or_default().push(r.clone());
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

/// Stale-free sets per block (MAY analysis for exit_frees).
/// A register is stale at a block's end iff some path to that block freed
/// it (a `!r` line) without redefining it afterwards. Unconditional defs
/// (before the first mid-block label) clear staleness; conditional defs
/// do not (some path bypasses them); any free establishes it. Edges come
/// from jmp/br lines (targets resolved via first-line labels); inner
/// labels (sw/assert chains) stay intra-block. The transfer is monotone
/// ((in - defs) u frees), so iterating from empty to a fixpoint
/// converges. A stale reg must not be freed at an exit (double-free on
/// the stale path); a non-stale reg keeps its exit free even if some
/// other path freed it, provided every such path redefined it since
/// (the loop back-edge case: free in the back-edge pred, redefinition
/// at the loop head/body before the exit).
fn stale_out_sets(body_lines: &[Vec<String>]) -> Vec<std::collections::BTreeSet<String>> {
    let is_label = |l: &str| {
        let t = l.trim();
        t.starts_with("L_") && t.ends_with(':') && !t.contains(' ')
    };
    let mut label_block: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        if let Some(first) = ls.first() {
            let t = first.trim();
            if is_label(t) {
                label_block.insert(t.trim_end_matches(':'), bi);
            }
        }
    }
    let mut succ: Vec<std::collections::BTreeSet<usize>> = vec![std::collections::BTreeSet::new(); body_lines.len()];
    for (bi, ls) in body_lines.iter().enumerate() {
        for l in ls {
            let t = l.trim_start();
            if let Some(rest) = t.strip_prefix("jmp ") {
                if let Some(&tb) = label_block.get(rest.trim()) {
                    if tb != bi {
                        succ[bi].insert(tb);
                    }
                }
            } else if let Some(rest) = t.strip_prefix("br ") {
                if let Some(arrow) = rest.find("->") {
                    for tgt in rest[arrow + 2..].split(',') {
                        if let Some(&tb) = label_block.get(tgt.trim()) {
                            if tb != bi {
                                succ[bi].insert(tb);
                            }
                        }
                    }
                }
            }
        }
    }
    let mut preds: Vec<Vec<usize>> = vec![Vec::new(); body_lines.len()];
    for (bi, ss) in succ.iter().enumerate() {
        for &s in ss {
            preds[s].push(bi);
        }
    }
    let cls: Vec<Vec<LineUse>> =
        body_lines.iter().map(|ls| ls.iter().map(|l| classify(l)).collect()).collect();
    let transfer = |bi: usize, mut stale: std::collections::BTreeSet<String>| -> std::collections::BTreeSet<String> {
        let mut conditional = false;
        for (li, u) in cls[bi].iter().enumerate() {
            if li > 0 && body_lines[bi].get(li).is_some_and(|l| is_label(l)) {
                conditional = true;
            }
            if let Some(d) = &u.def {
                if !conditional {
                    stale.remove(d);
                }
            }
            for f in &u.freed {
                stale.insert(f.clone());
            }
        }
        stale
    };
    let n = body_lines.len();
    let mut stale_in: Vec<std::collections::BTreeSet<String>> = vec![std::collections::BTreeSet::new(); n];
    let mut stale_out: Vec<std::collections::BTreeSet<String>> = vec![std::collections::BTreeSet::new(); n];
    loop {
        let mut changed = false;
        for bi in 0..n {
            let mut inn: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for &p in &preds[bi] {
                inn.extend(stale_out[p].iter().cloned());
            }
            if inn != stale_in[bi] {
                stale_in[bi] = inn;
                changed = true;
            }
            let out = transfer(bi, stale_in[bi].clone());
            if out != stale_out[bi] {
                stale_out[bi] = out;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    stale_out
}

/// Compute per-Return-block free lists. `dom_sets` holds raw dominator sets
/// (each incl. self) keyed by original block index; `body_lines` aligns by
/// the same index. `param_count` seeds params as pre-bound.
///
/// Soundness regime (all verified against `sa check` probes):
/// - DEFS count only from unconditional prefixes (block lines before the
///   first mid-block `L_...:` label): sw/assert chains create conditional
///   regions inside MIR blocks, and freeing a conditionally-defined reg at
///   a join exit is UnknownRegister on bypassing paths.
/// - MOVES and BORROWS count from ANYWHERE (even conditional): a consume
///   or borrow on any path vetoes exit-freeing (else UseAfterMove or
///   BorrowConflict on that path). FREES count per-path with
///   redefinition refresh (stale-may analysis, see stale_out_sets): a
///   free vetoes only if some path to this exit freed the reg without
///   redefining it afterwards (else double-free on that path); a reg
///   freed on a loop back-edge and redefined before the exit is NOT
///   stale, so the exit keeps its free (the loop end must never starve
///   the exit). Under-freeing is always safe (a leak, not a new trap).
/// - Candidates additionally need single definition in a dominator of the
///   exit; ordering is borrowers-before-sources over the borrow graph.
pub fn exit_frees(
    body_lines: &[Vec<String>],
    dom_sets: &[std::collections::HashSet<usize>],
    param_count: usize,
) -> BTreeMap<usize, Vec<String>> {
    // Split each block into unconditional prefix vs conditional rest.
    let is_label = |l: &str| {
        let t = l.trim();
        t.starts_with("L_") && t.ends_with(':') && !t.contains(' ')
    };
    // Per-block classified lines.
    let cls: Vec<Vec<LineUse>> = body_lines.iter().map(|ls| ls.iter().map(|l| classify(l)).collect()).collect();
    // Unconditional-prefix defs per block (dominance-gated candidates).
    // def_count per reg (whole function), move-consumed set, freed set.
    let mut def_count: BTreeMap<String, usize> = BTreeMap::new();
    // Def blocks per reg, unconditional prefixes only.
    let mut def_blocks: BTreeMap<String, BTreeSet<usize>> = BTreeMap::new();
    let mut moved: BTreeSet<String> = BTreeSet::new();
    // borrow edges borrower -> source (anywhere).
    let mut borrows: BTreeMap<String, String> = BTreeMap::new();
    for (bi, ls) in cls.iter().enumerate() {
        let mut conditional = false;
        for (li, u) in ls.iter().enumerate() {
            if li > 0 && body_lines[bi].get(li).is_some_and(|l| is_label(l)) {
                conditional = true;
            }
            if let Some(d) = &u.def {
                *def_count.entry(d.clone()).or_insert(0) += 1;
                if !conditional {
                    def_blocks.entry(d.clone()).or_default().insert(bi);
                }
            }
            for m in &u.move_srcs {
                moved.insert(m.clone());
            }
            if let Some((b, s)) = &u.borrow {
                borrows.insert(b.clone(), s.clone());
            }
        }
    }
    let stale_out = stale_out_sets(body_lines);
    let params: BTreeSet<String> = (1..=param_count).map(|i| format!("_{}", i)).collect();
    let mut out: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for (bi, ls) in body_lines.iter().enumerate() {
        // Return-terminated blocks only (last code line starts with return).
        let is_exit = ls.iter().any(|l| l.trim_start().starts_with("return"));
        if !is_exit {
            continue;
        }
        // Candidates: defined exactly once in an unconditional prefix of
        // a dominator of this exit (else the free could hit an unbound reg
        // on some path), never moved anywhere, not stale at this exit
        // (stale = freed on some path to this exit without a later
        // redefinition; a loop back-edge free followed by a redefinition
        // before the exit is NOT stale, so the exit keeps its free),
        // params included (unused params leak).
        // Dominator blocks of this exit (incl. itself).
        let doms: &std::collections::HashSet<usize> = &dom_sets[bi];
        let mut cand: BTreeSet<String> = def_count
            .iter()
            .filter(|(_, &n)| n == 1)
            .map(|(r, _)| r.clone())
            .filter(|r| {
                def_blocks.get(r).is_some_and(|bis| bis.iter().all(|b| doms.contains(b)))
            })
            .filter(|r| !moved.contains(r) && !stale_out[bi].contains(r))
            .collect();
        for p in &params {
            if !moved.contains(p) && !stale_out[bi].contains(p) {
                // Param must be defined (bound by header) — count it only if
                // it never appears as a def (params have no def lines).
                if !def_count.contains_key(p) {
                    cand.insert(p.clone());
                }
            }
        }
        // Borrow ordering: repeatedly emit borrows whose sources are not
        // (pending candidates), i.e. borrowers before sources. Sources that
        // are themselves borrowed stay until their borrowers are out.
        // Anything left after the fixpoint is skipped (ambiguous, e.g. the
        // source is moved/unbound — freeing would trap).
        let mut ordered: Vec<String> = vec![];
        let mut pending = cand;
        loop {
            // A candidate can go iff it is not a live source, or all its
            // borrowers are already ordered (or not candidates).
            let ready: Vec<String> = pending
                .iter()
                .filter(|r| {
                    // r is a source with a pending borrower still inside?
                    !borrows.iter().any(|(b, s)| s == *r && pending.contains(b))
                })
                .cloned()
                .collect();
            if ready.is_empty() {
                break;
            }
            for r in ready {
                pending.remove(&r);
                ordered.push(r);
            }
        }
        // `pending` leftovers are skipped (would trap).
        if !ordered.is_empty() {
            out.insert(bi, ordered);
        }
    }
    out
}

#[cfg(test)]
mod call_dest_frees_tests {
    use super::*;

    fn lines(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn frees_call_dest_after_single_print_use() {
        // 105 shape: from_str result printed once on a branch path.
        let body = vec![
            lines(&["L_bb0:", "jmp L_bb1"]),
            lines(&["L_bb1:", "_3 = call @from_str(_carg)", "jmp L_bb2"]),
            lines(&["L_bb2:", "call @print(_3)", "_2 = 0", "jmp L_bb6"]),
            lines(&["L_bb6:", "return 0"]),
        ];
        let out = call_dest_frees(&body);
        assert_eq!(out.get(&2), Some(&vec!["_3".to_string()]), "{:?}", out);
        assert_eq!(out.len(), 1, "{:?}", out);
    }

    #[test]
    fn vetoes_second_use_and_plain_move_def() {
        let body = vec![
            lines(&["L_bb0:", "_3 = call @f(0)", "jmp L_bb1"]),
            lines(&["L_bb1:", "call @g(_3)", "_x = _3", "jmp L_bb2"]),
            lines(&["L_bb2:", "return 0"]),
        ];
        assert!(call_dest_frees(&body).is_empty(), "{:?}", call_dest_frees(&body));
        let body = vec![
            lines(&["L_bb0:", "_3 = _1", "jmp L_bb1"]),
            lines(&["L_bb1:", "call @g(_3)", "jmp L_bb2"]),
            lines(&["L_bb2:", "return 0"]),
        ];
        assert!(call_dest_frees(&body).is_empty(), "{:?}", call_dest_frees(&body));
    }

    #[test]
    fn vetoes_join_mixing_freed_and_active_paths() {
        // 129 shape: def in the entry block, use on one branch only, and
        // the join is reachable from both sides -> freeing would make the
        // join see Freed vs Active. No free (baseline behavior kept).
        let body = vec![
            lines(&["L_bb0:", "_3 = call @f(0)", "br _c -> L_bb1, L_bb2"]),
            lines(&["L_bb1:", "call @g(_3)", "jmp L_bb3"]),
            lines(&["L_bb2:", "jmp L_bb3"]),
            lines(&["L_bb3:", "return 0"]),
        ];
        assert!(call_dest_frees(&body).is_empty(), "{:?}", call_dest_frees(&body));
        // 105 shape: the def block's only successor is the use block, so
        // no join can mix paths -> the free is kept.
        let body = vec![
            lines(&["L_bb0:", "jmp L_bb1"]),
            lines(&["L_bb1:", "_3 = call @f(0)", "jmp L_bb2"]),
            lines(&["L_bb2:", "call @g(_3)", "jmp L_bb3"]),
            lines(&["L_bb3:", "return 0"]),
        ];
        assert_eq!(
            call_dest_frees(&body).get(&2),
            Some(&vec!["_3".to_string()]),
            "{:?}",
            call_dest_frees(&body)
        );
    }

    #[test]
    fn vetoes_already_freed_and_mid_label_use_block() {
        let body = vec![
            lines(&["L_bb0:", "_3 = call @f(0)", "jmp L_bb1"]),
            lines(&["L_bb1:", "call @g(_3)", "!_3", "jmp L_bb2"]),
            lines(&["L_bb2:", "return 0"]),
        ];
        assert!(call_dest_frees(&body).is_empty(), "{:?}", call_dest_frees(&body));
        let body = vec![
            lines(&["L_bb0:", "_3 = call @f(0)", "jmp L_bb1"]),
            lines(&["L_bb1:", "call @g(_3)", "L_sw_bb1_0:", "jmp L_bb2"]),
            lines(&["L_bb2:", "return 0"]),
        ];
        assert!(call_dest_frees(&body).is_empty(), "{:?}", call_dest_frees(&body));
    }
}

#[cfg(test)]
mod drop_tests {
    use super::*;

    fn lines(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn temp_frees_block_local_only() {
        // A compare temp is read by its own eq/br — that is its use, not
        // liveness. It is released at its block's end only when no other block
        // mentions it, and never when a branch consumes it.
        let body = vec![
            lines(&[
                "L_bb0:",
                "_c = eq _1, 0",
                "_sw_bb0 = _c",
                "_sw_bb0_eq_0 = eq _sw_bb0, 0",
                "br _sw_bb0_eq_0 -> L_bb1, L_bb2",
            ]),
            lines(&["L_bb1:", "_x = 1", "jmp L_bb3"]),
            lines(&["L_bb2:", "_y = 2", "jmp L_bb3"]),
            lines(&["L_bb3:", "return 0"]),
        ];
        let out = temp_frees(&body);
        // The `br` consumes `_sw_bb0_eq_0`, so only the base temp is freed.
        assert_eq!(out.get(&0), Some(&vec!["_sw_bb0".to_string()]), "{:?}", out);
        assert!(!out.contains_key(&3), "{:?}", out);
    }

    #[test]
    fn temp_frees_skips_cross_block_and_branch_reads() {
        let body = vec![
            lines(&[
                "L_bb0:",
                "_as_bb0 = _1",
                "_as_bb0_eq = eq _as_bb0, 0",
                "br _as_bb0_eq -> L_bb1, L_as_bb0_fail",
                "L_as_bb0_fail:",
                "panic(1)",
            ]),
            lines(&["L_bb1:", "_as_bb0_eq = 3", "jmp L_bb2"]),
            lines(&["L_bb2:", "return 0"]),
        ];
        let out = temp_frees(&body);
        // `_as_bb0_eq` is redefined in bb1 (two blocks mention it) and is read
        // by the branch in bb0: both veto a release.
        assert_eq!(out.get(&0), Some(&vec!["_as_bb0".to_string()]), "{:?}", out);
    }

    #[test]
    fn mv_frees_branch_temp_after_last_use() {
        // 182 shape: bb1 binds the temp and consumes it, bb2 bypasses,
        // bb3 joins and returns. The temp is released after its use in bb1.
        let body = vec![
            lines(&["L_bb0:", "br _c -> L_bb1, L_bb2"]),
            lines(&[
                "L_bb1:",
                "_mv_bb1_0 = _16",
                "_11_v0 = eq _mv_bb1_0, 4",
                "jmp L_bb3",
            ]),
            lines(&["L_bb2:", "_11_v1 = 0", "jmp L_bb3"]),
            lines(&["L_bb3:", "return 0"]),
        ];
        let out = mv_frees(&body);
        assert_eq!(
            out.get(&1),
            Some(&vec![(2, "_mv_bb1_0".to_string())]),
            "{:?}",
            out
        );
        assert!(!out.contains_key(&3), "{:?}", out);
    }

    #[test]
    fn mv_frees_two_temps_one_use_line() {
        // 129 shape: two temps consumed by a single `and`.
        let body = vec![
            lines(&["L_bb0:", "br _c -> L_bb1, L_bb2"]),
            lines(&[
                "L_bb1:",
                "_mv_bb1_0 = _25",
                "_mv_bb1_1 = _26",
                "_27 = and _mv_bb1_0, _mv_bb1_1",
                "jmp L_bb3",
            ]),
            lines(&["L_bb2:", "jmp L_bb3"]),
            lines(&["L_bb3:", "return 0"]),
        ];
        let out = mv_frees(&body);
        assert_eq!(
            out.get(&1),
            Some(&vec![
                (3, "_mv_bb1_0".to_string()),
                (3, "_mv_bb1_1".to_string())
            ]),
            "{:?}",
            out
        );
    }

    #[test]
    fn mv_frees_skips_moved_borrowed_freed_and_cross_block() {
        let body = vec![
            lines(&["L_bb0:", "_mv_bb0_0 = _1", "_6 = _mv_bb0_0", "jmp L_bb2"]),
            lines(&["L_bb1:", "_mv_bb1_0 = _2", "_x = &_mv_bb1_0", "jmp L_bb2"]),
            lines(&[
                "L_bb2:",
                "_mv_bb2_0 = _3",
                "_y = eq _mv_bb2_0, 0",
                "!_mv_bb2_0",
                "jmp L_bb3",
            ]),
            lines(&["L_bb3:", "_z = eq _mv_bb0_0, 1", "return 0"]),
        ];
        // Moved (bb0), borrowed-from (bb1), already freed (bb2) all veto;
        // the bb0 temp is additionally mentioned cross-block (bb3).
        assert!(mv_frees(&body).is_empty(), "{:?}", mv_frees(&body));
    }

    #[test]
    fn mv_frees_skips_terminator_use() {
        // The temp only use is the branch itself: no label-less line may
        // follow the terminator, and the read-shape gate vetoes it anyway.
        let body = vec![
            lines(&["L_bb0:", "_mv_bb0_0 = _1", "br _mv_bb0_0 -> L_bb1, L_bb2"]),
            lines(&["L_bb1:", "return 0"]),
            lines(&["L_bb2:", "return 0"]),
        ];
        assert!(mv_frees(&body).is_empty(), "{:?}", mv_frees(&body));
    }

    #[test]
    fn classify_shapes() {
        let u = classify("    _x = _y");
        assert_eq!(u.def, Some("_x".to_string()));
        assert_eq!(u.move_srcs, vec!["_y".to_string()]);
        let u = classify("    _b = &_a");
        assert_eq!(u.borrow, Some(("_b".to_string(), "_a".to_string())));
        let u = classify("    !_a");
        assert_eq!(u.freed, vec!["_a".to_string()]);
        let u = classify("L_bb0:");
        assert!(u.def.is_none());
        let u = classify("    // comment");
        assert!(u.def.is_none());
        let u = classify("    _x = call @f(_y, 0)");
        assert_eq!(u.def, Some("_x".to_string()));
        assert!(u.move_srcs.is_empty());
    }

    #[test]
    fn frees_simple_leak() {
        // _a alloc'd, stored into (non-consuming), never freed -> freed.
        // _v stored (non-consuming) -> freed. _r returned... return line
        // itself is not a def; _r defined once, never moved -> freed too?
        // (_r IS used by return — return CONSUMES the reg (probe r4: `return
        // _r` needs no `!_r`, and `return 0` with a live _r leaks it, probe
        // r5), so _r is not a free candidate.)
        let body = vec![lines(&[
            "_a = alloc 8",
            "store _a+0, 71 as u8",
            "_r = 0",
            "return _r",
        ])];
        let doms = vec![std::collections::HashSet::from([0])];
        let out = exit_frees(&body, &doms, 0);
        let frees = &out[&0];
        assert!(frees.contains(&"_a".to_string()), "{:?}", frees);
        assert!(!frees.contains(&"_r".to_string()), "return consumes _r: {:?}", frees);
    }

    #[test]
    fn borrow_ordering() {
        // _b borrows _a: borrower freed first.
        let body = vec![lines(&["_a = alloc 8", "_b = &_a", "_r = 0", "return _r"])];
        let doms = vec![std::collections::HashSet::from([0])];
        let out = exit_frees(&body, &doms, 0);
        let frees = &out[&0];
        let pb = frees.iter().position(|r| r == "_b").unwrap();
        let pa = frees.iter().position(|r| r == "_a").unwrap();
        assert!(pb < pa, "{:?}", frees);
    }

    #[test]
    fn moved_not_freed() {
        // _v moved into _w: _v must NOT be freed (would be UseAfterMove).
        let body = vec![lines(&["_v = 7", "_w = _v", "_r = 0", "return _r"])];
        let doms = vec![std::collections::HashSet::from([0])];
        let out = exit_frees(&body, &doms, 0);
        let frees = &out[&0];
        assert!(!frees.contains(&"_v".to_string()), "{:?}", frees);
        assert!(frees.contains(&"_w".to_string()), "{:?}", frees);
    }

    #[test]
    fn branch_local_never_freed_at_join() {
        // _x defined only in bb1 (branch); the join bb3 exit must NOT free
        // it (unbound on the bb2 path -> UnknownRegister). Regression test
        // for the _constant_0_ incident.
        let body = vec![
            lines(&["br _c -> L_bb1, L_bb2"]),
            lines(&["L_bb1:", "_x = 7", "jmp L_bb3"]),
            lines(&["L_bb2:", "jmp L_bb3"]),
            lines(&["L_bb3:", "_r = 0", "return _r"]),
        ];
        let doms = vec![
            std::collections::HashSet::from([0]),
            std::collections::HashSet::from([0, 1]),
            std::collections::HashSet::from([0, 2]),
            std::collections::HashSet::from([0, 3]),
        ];
        let out = exit_frees(&body, &doms, 0);
        // No candidates left (return consumes _r) -> no entry at all.
        let frees = out.get(&3).cloned().unwrap_or_default();
        assert!(!frees.contains(&"_x".to_string()), "{:?}", frees);
    }

    #[test]
    fn move_frees_branch_local() {
        // 129 shape: _m0 is a plain move in a branch block, read by a
        // compound op in the same block, never read elsewhere. The
        // branch block does not dominate the exit, so exit_frees would
        // skip it; move_frees releases it at the branch block's end.
        let body = vec![
            lines(&["L_bb0:", "_c = eq _1, 1", "br _c -> L_bb1, L_bb2"]),
            lines(&["L_bb1:", "_t = eq 2, -1", "_m0 = _t", "_u = and _m0, 3", "jmp L_bb3"]),
            lines(&["L_bb2:", "jmp L_bb3"]),
            lines(&["L_bb3:", "return 0"]),
        ];
        let out = move_frees(&body);
        assert_eq!(out.get(&1), Some(&vec!["_m0".to_string()]), "{:?}", out);
        assert!(!out.contains_key(&0), "{:?}", out);
        assert!(!out.contains_key(&3), "{:?}", out);
    }

    #[test]
    fn move_frees_skips_moved_crossblock_br_and_midlabel() {
        // Moved: _m is consumed by a later plain move -> freeing it
        // would be UseAfterMove. (_x, the move's dest, IS a candidate
        // and is the only free in this block.)
        let body = vec![lines(&["L_bb0:", "_m = _t", "_x = _m", "return 0"])];
        let out = move_frees(&body);
        assert_eq!(out.get(&0), Some(&vec!["_x".to_string()]), "{:?}", out);
        assert!(!out.values().flatten().any(|r| r == "_m"), "{:?}", out);
        // Cross-block: _m is read in another block -> not block-local.
        let body = vec![
            lines(&["L_bb0:", "jmp L_bb1"]),
            lines(&["L_bb1:", "_m = _t", "jmp L_bb2"]),
            lines(&["L_bb2:", "_u = eq _m, 0", "return 0"]),
        ];
        assert!(move_frees(&body).is_empty(), "{:?}", move_frees(&body));
        // Branch-read: _m is the br condition -> must outlive the block.
        let body = vec![
            lines(&["L_bb0:", "_m = _t", "br _m -> L_bb1, L_bb2"]),
            lines(&["L_bb1:", "jmp L_bb3"]),
            lines(&["L_bb2:", "jmp L_bb3"]),
            lines(&["L_bb3:", "return 0"]),
        ];
        assert!(move_frees(&body).is_empty(), "{:?}", move_frees(&body));
        // Mid-block label (assert fail arm): the block-end insertion
        // point would sit on the fail arm only, not the main path.
        let body = vec![
            lines(&["L_bb0:", "_m = _t", "br _c -> L_bb1, L_as_fail", "L_as_fail:", "panic(1)"]),
            lines(&["L_bb1:", "return 0"]),
        ];
        assert!(move_frees(&body).is_empty(), "{:?}", move_frees(&body));
        // Borrow source: liveness extends through the borrower.
        let body = vec![lines(&["L_bb0:", "_m = _t", "_b = &_m", "return 0"])];
        assert!(move_frees(&body).is_empty(), "{:?}", move_frees(&body));
        // Compare-temp domain: _as_ base temps stay with temp_frees.
        let body = vec![lines(&["L_bb0:", "_as_bb0 = _t", "jmp L_bb1"]), lines(&["L_bb1:", "return 0"])];
        assert!(move_frees(&body).is_empty(), "{:?}", move_frees(&body));
    }

    #[test]
    fn exit_frees_loop_backedge_redefined_kept() {
        // 09 exit shape, minimal: bb4 frees _11/_12 on the back-edge
        // path, bb2 redefines _12 before the exit, bb1 never redefines
        // _11. At the exit bb5, _12 is NOT stale (kept) and _11 IS
        // stale (skipped); _4/_6 were never freed (kept).
        let body = vec![
            lines(&["L_bb0:", "_4 = _p", "_6 = _w", "jmp L_bb1"]),
            lines(&["L_bb1:", "_11 = &_4", "jmp L_bb2"]),
            lines(&["L_bb2:", "_12 = &_6", "jmp L_bb3"]),
            lines(&["L_bb3:", "br _c -> L_bb4, L_bb5"]),
            lines(&["L_bb4:", "!_11", "!_12", "jmp L_bb2"]),
            lines(&["L_bb5:", "return 0"]),
        ];
        let doms: Vec<std::collections::HashSet<usize>> = vec![
            std::collections::HashSet::from([0]),
            std::collections::HashSet::from([0, 1]),
            std::collections::HashSet::from([0, 1, 2]),
            std::collections::HashSet::from([0, 1, 2, 3]),
            std::collections::HashSet::from([0, 1, 2, 3, 4]),
            std::collections::HashSet::from([0, 1, 2, 3, 5]),
        ];
        let frees = exit_frees(&body, &doms, 0);
        let got: std::collections::BTreeSet<String> =
            frees.get(&5).cloned().unwrap_or_default().into_iter().collect();
        assert_eq!(got, std::collections::BTreeSet::from(["_4".to_string(), "_6".to_string(), "_12".to_string()]));
    }

    #[test]
    fn exit_frees_straightline_freed_skipped() {
        // Freed unconditionally before the exit, never redefined:
        // stale, so the exit must not free it again.
        let body = vec![
            lines(&["L_bb0:", "_1 = _t", "!_1", "jmp L_bb1"]),
            lines(&["L_bb1:", "return 0"]),
        ];
        let doms: Vec<std::collections::HashSet<usize>> = vec![
            std::collections::HashSet::from([0]),
            std::collections::HashSet::from([0, 1]),
        ];
        let frees = exit_frees(&body, &doms, 0);
        assert!(frees.values().all(|v| !v.contains(&"_1".to_string())));
    }

    #[test]
    fn exit_frees_branch_freed_skipped() {
        // Freed on SOME path to the exit (MAY-stale): keep the old
        // conservative skip (a leak on the other path, never a
        // double-free on the freed path).
        let body = vec![
            lines(&["L_bb0:", "_1 = _t", "br _c -> L_bb1, L_bb2"]),
            lines(&["L_bb1:", "!_1", "jmp L_bb3"]),
            lines(&["L_bb2:", "jmp L_bb3"]),
            lines(&["L_bb3:", "return 0"]),
        ];
        let doms: Vec<std::collections::HashSet<usize>> = vec![
            std::collections::HashSet::from([0]),
            std::collections::HashSet::from([0, 1]),
            std::collections::HashSet::from([0, 2]),
            std::collections::HashSet::from([0, 3]),
        ];
        let frees = exit_frees(&body, &doms, 0);
        assert!(frees.values().all(|v| !v.contains(&"_1".to_string())));
    }
}
