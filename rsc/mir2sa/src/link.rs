//! mir2sa::link — merge several mir.json files into one .sa.
//!
//! This is the module-tree / multi-crate linking step (stream 8):
//!
//! - Input: one mir.json per crate, in dependency order (deps first, the
//!   bin last). Each mir.json comes from one `sars` run on that crate's
//!   root; rustc itself already resolved that crate's `mod` tree, so one
//!   mir.json already contains every module's functions.
//! - Identity: sars emits each function with its crate-relative def path
//!   (`m1::compute`, `main`) plus a top-level `crate` name. A definition's
//!   identity is `crate::path` (already-qualified paths pass through).
//!   Call sites carry `func_raw`: crate-relative for same-crate calls,
//!   crate-qualified for cross-crate calls (`mylib::triple`). Resolution
//!   tries, in order: exact identity, same-file bare path, globally
//!   unique bare path. The same-file step is what disambiguates two
//!   crates that share a module layout (`acrate::m::compute` vs
//!   `bcrate::m::compute`) for a call written inside one of them.
//! - Naming (Jev jev-1.14.0, qualify-on-collision, p=0.86): a function
//!   keeps its short name unless that name is defined more than once in
//!   the linked set. Colliding definitions are renamed to a qualified
//!   form derived from their identity (`mylib::a::helper` ->
//!   `mylib__a__helper`, `__` so it can never equal a plain `mylib_a`
//!   short name); call sites are rewritten to the resolved definition's
//!   final name. Non-colliding output is therefore identical to plain
//!   `lower` output, and single-crate `lower` is untouched.
//! - Entry point: the definition with path `main` in a crate-named file
//!   (or, for path-only legacy input, a two-segment path ending in
//!   `main`) keeps the bare name `main`. Two entry mains (two bins in
//!   one link) is a loud link error, never a silent pick. A module fn
//!   literally named `main` (`m::main`) is qualified like any other
//!   collision.
//! - Cross-crate short-name fix: sars emits a Debug-fallback name
//!   (`mylib_triple`) for extern-crate calls while the lib crate defines
//!   plain `triple`; without rewriting, the bin would call an `@extern`
//!   stub and the lib body would never run (silently wrong). Link
//!   rewrites such calls to the definition's final name.
//! - Limits (loud, not silent): `Rvalue::Call` carries no func_raw (it
//!   only arises from the `parse` path, never from sars output) and is
//!   left unchanged. `func_raw` is truncated to 100 chars by sars, so a
//!   100-char raw path resolves only on a unique prefix match; an
//!   ambiguous truncation warns and leaves the call unchanged. Inputs
//!   without def paths (old mir.json, `parse` output) fall back to
//!   `link<file-index>__<name>` qualification and cannot keep an entry
//!   main through a collision; that case warns. A merged .sa's mir.json
//!   is terminal: re-linking it is unsupported (its `crate` is unset
//!   and its calls are already rewritten).

use crate::mir::{MirFile, Term};
use crate::render_util::sa_ident;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::ExitCode;

/// Result of resolving one link: the merged mir.json plus accounting.
pub struct Linked {
    pub mir: MirFile,
    pub renamed: usize,
    pub warnings: Vec<String>,
}

/// Qualified SA name for a full def path: segments sanitized, joined
/// with `__`.
pub fn qualified_name(path: &str) -> String {
    path.split("::")
        .map(sa_ident)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("__")
}

/// Definition identity: `crate::path` (already-qualified paths pass
/// through unchanged).
fn def_identity(krate: Option<&str>, path: &str) -> String {
    match krate {
        Some(c) if !path.starts_with(&format!("{}::", c)) => format!("{}::{}", c, path),
        _ => path.to_string(),
    }
}

/// Entry-point main: path `main` when the crate is known; for legacy
/// path-only input, a two-segment path ending in `main`.
fn is_entry_main(path: &str, krate: Option<&str>) -> bool {
    if krate.is_some() {
        return path == "main";
    }
    let segs: Vec<&str> = path.split("::").collect();
    segs.len() == 2 && segs[1] == "main"
}

struct Def {
    file: usize,
    short: String,
    path: Option<String>,
    identity: Option<String>,
}

/// Resolve names across all input files. Returns the merged MirFile with
/// renamed definitions and rewritten call sites, or a loud error for a
/// duplicate entry main.
pub fn resolve_link(files: &[MirFile]) -> Result<Linked, String> {
    let mut defs: Vec<Def> = vec![];
    for (fi, f) in files.iter().enumerate() {
        for fun in &f.functions {
            let identity = fun
                .path
                .as_deref()
                .map(|p| def_identity(f.crate_name.as_deref(), p));
            defs.push(Def {
                file: fi,
                short: sa_ident(&fun.name),
                path: fun.path.clone(),
                identity,
            });
        }
    }
    let mut by_short: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (d, def) in defs.iter().enumerate() {
        by_short.entry(def.short.clone()).or_default().push(d);
    }
    let mut final_name: Vec<String> = defs.iter().map(|d| d.short.clone()).collect();
    let mut renamed = 0usize;
    let mut warnings: Vec<String> = vec![];
    for (short, group) in &by_short {
        if group.len() == 1 {
            continue;
        }
        let krate_of = |d: usize| files[defs[d].file].crate_name.as_deref();
        let mains: Vec<usize> = group
            .iter()
            .copied()
            .filter(|&d| {
                defs[d]
                    .path
                    .as_deref()
                    .is_some_and(|p| is_entry_main(p, krate_of(d)))
            })
            .collect();
        if mains.len() > 1 {
            return Err(format!(
                "duplicate entry-point main: {} definitions share the short name `main` \
                 (link at most one bin crate; other mains must live in modules)",
                mains.len()
            ));
        }
        for &d in group {
            let is_main = defs[d]
                .path
                .as_deref()
                .is_some_and(|p| is_entry_main(p, krate_of(d)));
            if is_main {
                continue; // the entry point keeps the bare name
            }
            let q = match &defs[d].identity {
                Some(id) => qualified_name(id),
                None => {
                    warnings.push(format!(
                        "definition `{}` has no def path; qualifying as link{}__{} \
                         (entry-main detection unavailable for path-less input)",
                        short, defs[d].file, short
                    ));
                    format!("link{}__{}", defs[d].file, short)
                }
            };
            if q != *short {
                renamed += 1;
            }
            final_name[d] = q;
        }
    }
    // Uniqueness: a qualified name may still equal another final name
    // (e.g. a plain `a__b` fn next to `a::b`). First occurrence (in link
    // order) keeps the name; later ones get __2, __3, ... deterministically.
    let mut used: HashSet<String> = HashSet::new();
    for name in final_name.iter_mut() {
        if used.contains(name.as_str()) {
            let base = name.clone();
            let mut n = 2usize;
            let mut cand = format!("{}__{}", base, n);
            while used.contains(cand.as_str()) {
                n += 1;
                cand = format!("{}__{}", base, n);
            }
            warnings.push(format!(
                "name `{}` already used; a later definition renamed to `{}`",
                base, cand
            ));
            *name = cand;
            renamed += 1;
        }
        used.insert(name.clone());
    }
    // Resolution maps. A repeated identity means the same crate was
    // linked twice: calls resolve to the first definition, with a warning.
    let mut by_identity: HashMap<String, String> = HashMap::new();
    let mut identities: Vec<String> = vec![];
    let mut by_file_bare: HashMap<(usize, String), String> = HashMap::new();
    let mut bare_counts: HashMap<String, usize> = HashMap::new();
    for (d, def) in defs.iter().enumerate() {
        if let Some(id) = &def.identity {
            if by_identity.contains_key(id) {
                warnings.push(format!(
                    "def identity `{}` appears more than once in the link inputs; \
                     calls resolve to the first definition",
                    id
                ));
            } else {
                by_identity.insert(id.clone(), final_name[d].clone());
                identities.push(id.clone());
            }
        }
        if let Some(p) = &def.path {
            by_file_bare
                .entry((def.file, p.clone()))
                .or_insert_with(|| final_name[d].clone());
            *bare_counts.entry(p.clone()).or_default() += 1;
        }
    }
    let resolve_call = |caller_file: usize,
                        raw: &str,
                        warnings: &mut Vec<String>|
     -> Option<String> {
        if let Some(f) = by_identity.get(raw) {
            return Some(f.clone());
        }
        if let Some(f) = by_file_bare.get(&(caller_file, raw.to_string())) {
            return Some(f.clone());
        }
        if bare_counts.get(raw).is_some_and(|&c| c == 1) {
            for (d, def) in defs.iter().enumerate() {
                if def.path.as_deref() == Some(raw) {
                    return Some(final_name[d].clone());
                }
            }
        }
        // sars truncates func_raw to 100 chars: a 100-char raw path may be
        // a prefix of the real identity. Resolve only on a unique prefix.
        if raw.len() == 100 {
            let cands: Vec<&String> = identities.iter().filter(|p| p.starts_with(raw)).collect();
            if cands.len() == 1 {
                return by_identity.get(cands[0]).cloned();
            }
            if cands.len() > 1 {
                warnings.push(format!(
                    "truncated call path `{}` matches {} definitions; call left unchanged",
                    raw,
                    cands.len()
                ));
            }
        }
        None
    };
    // Merge: rename definitions, rewrite call sites through func_raw.
    let mut functions = vec![];
    let mut di = 0usize;
    for (fi, f) in files.iter().enumerate() {
        for fun in &f.functions {
            let mut fun = fun.clone();
            fun.name = final_name[di].clone();
            di += 1;
            for b in fun.blocks.iter_mut() {
                if let Term::Call {
                    func, func_raw, ..
                } = &mut b.terminator
                {
                    if let Some(raw) = func_raw.clone() {
                        if let Some(resolved) = resolve_call(fi, &raw, &mut warnings) {
                            if resolved != *func {
                                *func = resolved;
                            }
                        }
                    }
                }
            }
            functions.push(fun);
        }
    }
    let source = files
        .iter()
        .find_map(|f| f.source.clone())
        .or_else(|| Some("mir2sa link".to_string()));
    Ok(Linked {
        // Multi-crate output: no single crate name; this mir.json is
        // terminal (for .sa rendering, not for re-linking).
        mir: MirFile {
            source,
            crate_name: None,
            functions,
        },
        renamed,
        warnings,
    })
}

/// `mir2sa link <a.mir.json> [<b.mir.json> ...] --out <out.sa> [--strict]`
pub fn cmd_link(args: &[String]) -> ExitCode {
    let (mut inputs, mut out, mut strict) = (vec![], None, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = args.get(i).cloned();
            }
            "--strict" => strict = true,
            s if !s.starts_with("--") => inputs.push(s.to_string()),
            _ => {}
        }
        i += 1;
    }
    let out = match out {
        Some(o) => o,
        None => {
            eprintln!("usage: mir2sa link <a.mir.json> [<b.mir.json> ...] --out <out.sa> [--strict]");
            return ExitCode::from(2);
        }
    };
    if inputs.is_empty() {
        eprintln!("usage: mir2sa link <a.mir.json> [<b.mir.json> ...] --out <out.sa> [--strict]");
        return ExitCode::from(2);
    }
    let mut files: Vec<MirFile> = vec![];
    for input in &inputs {
        let text = match std::fs::read_to_string(input) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("read {}: {}", input, e);
                return ExitCode::from(2);
            }
        };
        match serde_json::from_str(&text) {
            Ok(m) => files.push(m),
            Err(e) => {
                eprintln!("bad mir.json {}: {}", input, e);
                return ExitCode::from(2);
            }
        }
    }
    let linked = match resolve_link(&files) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("link: {}", e);
            return ExitCode::from(1);
        }
    };
    for w in &linked.warnings {
        eprintln!("link warning: {}", w);
    }
    // Same SSA versioning as `lower`, then the shared .sa renderer.
    let versioned = MirFile {
        source: linked.mir.source.clone(),
        crate_name: None,
        functions: linked
            .mir
            .functions
            .iter()
            .map(crate::version::version_function)
            .collect(),
    };
    let (sa, unsup) = crate::lower::render_sa(&versioned);
    if std::fs::write(&out, &sa).is_err() {
        eprintln!("write {}", out);
        return ExitCode::from(2);
    }
    println!(
        "linked {} UNSUPPORTED={} renamed={}",
        out,
        unsup.len(),
        linked.renamed
    );
    for u in &unsup {
        eprintln!("  UNSUPPORTED: {}", u);
    }
    if strict && !unsup.is_empty() {
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod link_tests {
    use super::*;
    use crate::mir::{Block, Function, Operand, Stmt};

    fn blank_fn(name: &str, path: Option<&str>, term: Term) -> Function {
        Function {
            name: name.to_string(),
            path: path.map(|s| s.to_string()),
            locals: vec![], local_types: Default::default(),
            params: vec![],
            ret: Some("i32".to_string()),
            sig_ok: true,
            blocks: vec![Block {
                id: "bb0".to_string(),
                statements: vec![],
                terminator: term,
            }],
        }
    }

    fn file(krate: Option<&str>, functions: Vec<Function>) -> MirFile {
        MirFile {
            source: None,
            crate_name: krate.map(|s| s.to_string()),
            functions,
        }
    }

    fn ret() -> Term {
        Term::Return { ret: None }
    }

    fn call(func: &str, raw: Option<&str>) -> Term {
        Term::Call {
            func: func.to_string(),
            func_raw: raw.map(|s| s.to_string()),
            args: vec![],
            dest: None,
            target: Some("bb0".to_string()),
            sig: None,
        }
    }

    fn names(mir: &MirFile) -> Vec<String> {
        mir.functions.iter().map(|f| f.name.clone()).collect()
    }

    fn call_func(f: &Function) -> &str {
        if let Term::Call { func, .. } = &f.blocks[0].terminator {
            func
        } else {
            panic!("expected a Call terminator");
        }
    }

    #[test]
    fn qualified_name_joins_with_double_underscore() {
        assert_eq!(qualified_name("mylib::a::helper"), "mylib__a__helper");
        assert_eq!(qualified_name("mybin::main"), "mybin__main");
    }

    #[test]
    fn non_colliding_names_unchanged() {
        let files = vec![file(
            Some("mybin"),
            vec![
                blank_fn("triple", Some("triple"), ret()),
                blank_fn("main", Some("main"), ret()),
            ],
        )];
        let linked = resolve_link(&files).unwrap();
        assert_eq!(names(&linked.mir), vec!["triple", "main"]);
        assert_eq!(linked.renamed, 0);
    }

    #[test]
    fn same_name_across_modules_qualified_and_calls_rewritten() {
        let caller = blank_fn("main", Some("main"), call("compute", Some("m1::compute")));
        let files = vec![file(
            Some("mybin"),
            vec![
                blank_fn("compute", Some("m1::compute"), ret()),
                blank_fn("compute", Some("m2::compute"), ret()),
                caller,
            ],
        )];
        let linked = resolve_link(&files).unwrap();
        assert_eq!(
            names(&linked.mir),
            vec!["mybin__m1__compute", "mybin__m2__compute", "main"]
        );
        assert_eq!(linked.renamed, 2);
        assert_eq!(call_func(&linked.mir.functions[2]), "mybin__m1__compute");
    }

    #[test]
    fn cross_crate_call_normalized_to_definition() {
        // Bin calls mylib::triple; sars gave the call the Debug-fallback
        // short name `mylib_triple` while the lib defines `triple`.
        let bin_main = blank_fn("main", Some("main"), call("mylib_triple", Some("mylib::triple")));
        let files = vec![
            file(Some("mylib"), vec![blank_fn("triple", Some("triple"), ret())]),
            file(Some("mybin"), vec![bin_main]),
        ];
        let linked = resolve_link(&files).unwrap();
        assert_eq!(names(&linked.mir), vec!["triple", "main"]);
        assert_eq!(call_func(&linked.mir.functions[1]), "triple");
    }

    #[test]
    fn same_module_layout_in_two_crates_disambiguated() {
        // Both crates define m::compute. An identity call from the bin
        // resolves to acrate's; a bare call inside acrate resolves to
        // acrate's own (same-file step), never bcrate's.
        let a_caller = blank_fn("use_it", Some("use_it"), call("compute", Some("m::compute")));
        let bin_main = blank_fn(
            "main",
            Some("main"),
            call("compute", Some("acrate::m::compute")),
        );
        let files = vec![
            file(
                Some("acrate"),
                vec![
                    blank_fn("compute", Some("m::compute"), ret()),
                    a_caller,
                ],
            ),
            file(Some("bcrate"), vec![blank_fn("compute", Some("m::compute"), ret())]),
            file(Some("mybin"), vec![bin_main]),
        ];
        let linked = resolve_link(&files).unwrap();
        assert_eq!(
            names(&linked.mir),
            vec![
                "acrate__m__compute",
                "use_it",
                "bcrate__m__compute",
                "main"
            ]
        );
        assert_eq!(call_func(&linked.mir.functions[1]), "acrate__m__compute");
        assert_eq!(call_func(&linked.mir.functions[3]), "acrate__m__compute");
    }

    #[test]
    fn entry_main_kept_bare_among_module_mains() {
        let files = vec![file(
            Some("mybin"),
            vec![
                blank_fn("main", Some("main"), ret()),
                blank_fn("main", Some("m::main"), ret()),
            ],
        )];
        let linked = resolve_link(&files).unwrap();
        assert_eq!(names(&linked.mir), vec!["main", "mybin__m__main"]);
    }

    #[test]
    fn duplicate_entry_main_is_loud_error() {
        let files = vec![
            file(Some("abin"), vec![blank_fn("main", Some("main"), ret())]),
            file(Some("bbin"), vec![blank_fn("main", Some("main"), ret())]),
        ];
        assert!(resolve_link(&files).is_err());
    }

    #[test]
    fn unknown_call_left_unchanged() {
        // std/external calls have no linked definition; they keep their
        // short name so the existing @extern closure still applies.
        let f = blank_fn(
            "main",
            Some("main"),
            call("std_io__print", Some("std::io::_print")),
        );
        let files = vec![file(Some("mybin"), vec![f])];
        let linked = resolve_link(&files).unwrap();
        assert_eq!(call_func(&linked.mir.functions[0]), "std_io__print");
        // Unused import guard: Stmt/Operand stay part of the test surface.
        let _ = Stmt::Nop {
            text: "x".to_string(),
        };
        let _ = Operand::Copy {
            place: "_1".to_string(),
        off: 0, off2: 0, deref: false, deref_first: false, pty: None, proj_loud: false , deref_last: false };
    }
}
