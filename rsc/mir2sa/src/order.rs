//! mir2sa::order — RPO block ordering for emission.
//!
//! SA checks definition-before-use in TEXTUAL order (single forward scan:
//! a use textually before its def is UnknownRegister even across a loop
//! back-edge). MIR block order is arbitrary (optimized layout), so emission
//! follows reverse postorder from the entry block: RPO numbers respect
//! dominance, hence every SSA def textually precedes its uses. Unreachable
//! blocks append in original order. Jumps stay explicit; order changes
//! nothing semantic.
use std::collections::{HashMap, HashSet};

use crate::mir::{Block, Term};

fn successors(t: &Term) -> Vec<&str> {
    match t {
        Term::Goto { target } => vec![target],
        Term::Call { target, .. } => target.iter().map(|s| s.as_str()).collect(),
        Term::Drop { target, .. } => vec![target],
        Term::SwitchInt { targets, otherwise, .. } => {
            let mut v: Vec<&str> = targets.iter().map(|(_, t)| t.as_str()).collect();
            v.push(otherwise);
            v
        }
        Term::Assert { target, .. } => vec![target],
        Term::InlineAsm { target, .. } => target.iter().map(|s| s.as_str()).collect(),
        _ => vec![],
    }
}

fn dfs(
    i: usize,
    blocks: &[Block],
    index: &HashMap<&str, usize>,
    visited: &mut HashSet<usize>,
    post: &mut Vec<usize>,
) {
    if !visited.insert(i) {
        return;
    }
    for s in successors(&blocks[i].terminator) {
        if let Some(&j) = index.get(s) {
            dfs(j, blocks, index, visited, post);
        }
    }
    post.push(i);
}

pub fn rpo_order(blocks: &[Block]) -> Vec<usize> {
    if blocks.is_empty() {
        return vec![];
    }
    let index: HashMap<&str, usize> =
        blocks.iter().enumerate().map(|(i, b)| (b.id.as_str(), i)).collect();
    let mut visited = HashSet::new();
    let mut post = vec![];
    dfs(0, blocks, &index, &mut visited, &mut post);
    post.reverse();
    for (i, _) in blocks.iter().enumerate() {
        if !visited.contains(&i) {
            post.push(i);
        }
    }
    post
}

#[cfg(test)]
mod order_tests {
    use super::*;

    fn blk(id: &str, term: Term) -> Block {
        Block { id: id.to_string(), statements: vec![], terminator: term }
    }

    #[test]
    fn rpo_loop_back_edge() {
        // entry -> body -> latch -> body; latch textually after body in MIR.
        let blocks = vec![
            blk("bb0", Term::Goto { target: "bb1".to_string() }),
            blk("bb1", Term::Goto { target: "bb2".to_string() }),
            blk("bb2", Term::Goto { target: "bb1".to_string() }),
        ];
        assert_eq!(rpo_order(&blocks), vec![0, 1, 2]);
    }

    #[test]
    fn rpo_diamond() {
        let blocks = vec![
            blk("bb0", Term::Goto { target: "bb2".to_string() }),
            blk("bb9", Term::Goto { target: "bb2".to_string() }),
            blk("bb2", Term::Return { ret: None }),
        ];
        let order = rpo_order(&blocks);
        // Entry first, join after its reachable predecessor.
        assert_eq!(order[0], 0);
        assert!(order.iter().position(|&i| i == 2).unwrap() > order.iter().position(|&i| i == 0).unwrap());
    }

    #[test]
    fn rpo_unreachable_appended() {
        let blocks = vec![
            blk("bb0", Term::Return { ret: None }),
            blk("bb9", Term::Return { ret: None }),
        ];
        assert_eq!(rpo_order(&blocks), vec![0, 1]);
    }

    #[test]
    fn dom_seeds_diamond() {
        use crate::mir::{Operand, Rvalue, Stmt};
        // Diamond: bb0 defines _1 and branches; bb1/bb2 define their own
        // exclusive temps. The join bb3 sees dominator defs (_1) but never
        // exclusive-branch defs (_2, _3) — so joins stay legal.
        let mk = |id: &str, dest: Option<&str>, targets: Vec<String>, otherwise: &str| Block {
            id: id.to_string(),
            statements: dest
                .map(|d| Stmt::Assign {
                    dest: d.to_string(),
                    dest_place: Some(d.to_string()),
                    rvalue: Rvalue::Use { op: Operand::Copy { place: "_9".to_string() } },
                })
                .into_iter()
                .collect(),
            terminator: Term::SwitchInt {
                discr: Box::new(Operand::Copy { place: "_9".to_string() }),
                targets: targets.into_iter().map(|t| ("0".to_string(), t)).collect(),
                otherwise: otherwise.to_string(),
            },
        };
        let goto = |id: &str, target: &str| Block {
            id: id.to_string(),
            statements: vec![],
            terminator: Term::Goto { target: target.to_string() },
        };
        let blocks = vec![
            mk("bb0", Some("_1"), vec!["bb1".to_string()], "bb2"),
            mk("bb1", Some("_2"), vec![], "bb3"),
            mk("bb2", Some("_3"), vec![], "bb3"),
            goto("bb3", "bb3"),
        ];
        let seeds = super::dom_seeds(&blocks, 0);
        assert!(seeds[0].is_empty());
        assert!(seeds[1].contains("_1"));
        assert!(seeds[2].contains("_1"));
        assert!(seeds[3].contains("_1"), "dominator def visible: {:?}", seeds[3]);
        assert!(!seeds[3].contains("_2"), "exclusive branch stays legal: {:?}", seeds[3]);
        assert!(!seeds[3].contains("_3"), "exclusive branch stays legal: {:?}", seeds[3]);
    }
}

/// Raw dominator sets (each incl. self) over explicit terminator edges.
/// Entry root is blocks[0]; unreachable blocks end with the full set.
pub fn dom_sets(blocks: &[Block]) -> Vec<HashSet<usize>> {
    let n = blocks.len();
    let index: HashMap<&str, usize> =
        blocks.iter().enumerate().map(|(i, b)| (b.id.as_str(), i)).collect();
    let mut succ: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, b) in blocks.iter().enumerate() {
        for s in successors(&b.terminator) {
            if let Some(&j) = index.get(s) {
                succ[i].push(j);
            }
        }
    }
    let mut pred: Vec<Vec<usize>> = vec![vec![]; n];
    for (i, ss) in succ.iter().enumerate() {
        for &j in ss {
            pred[j].push(i);
        }
    }
    let all: HashSet<usize> = (0..n).collect();
    let mut dom: Vec<HashSet<usize>> = vec![all.clone(); n];
    if n > 0 {
        dom[0] = HashSet::from([0]);
        let mut changed = true;
        while changed {
            changed = false;
            for i in 1..n {
                let mut new: Option<HashSet<usize>> = None;
                for &p in &pred[i] {
                    new = Some(match new {
                        None => dom[p].clone(),
                        Some(s) => s.intersection(&dom[p]).cloned().collect(),
                    });
                }
                let mut new = new.unwrap_or_default();
                new.insert(i);
                if new != dom[i] {
                    dom[i] = new;
                    changed = true;
                }
            }
        }
    }
    dom
}

/// Bound seeds per block: header params plus Assign/Call dests of all
/// STRICT dominators. A rebind of a seeded dest is a same-path redefinition
/// (RegisterRedefinition downstream) and goes loud; exclusive-branch joins
/// never seed each other, so they stay legal. Unreachable blocks seed from
/// params only.
pub fn dom_seeds(blocks: &[Block], n_params: usize) -> Vec<std::collections::BTreeSet<String>> {
    use std::collections::BTreeSet;
    let dom = dom_sets(blocks);
    let n = blocks.len();
    let mut defs: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    for (i, b) in blocks.iter().enumerate() {
        for s in &b.statements {
            if let crate::mir::Stmt::Assign { dest, .. } = s {
                defs[i].insert(dest.clone());
            }
        }
        if let Term::Call { dest: Some(d), .. } = &b.terminator {
            defs[i].insert(d.clone());
        }
    }
    // Seed = params + defs of strict dominators.
    let params: BTreeSet<String> = (1..=n_params).map(|i| format!("_{}", i)).collect();
    (0..n)
        .map(|i| {
            let mut s = params.clone();
            for &d in &dom[i] {
                if d != i {
                    s.extend(defs[d].iter().cloned());
                }
            }
            s
        })
        .collect()
}
