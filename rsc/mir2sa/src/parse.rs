//! mir2sa::parse.
use crate::mir::*;

// ---------------------------------------------------------------------------
// parse: real `-Zunpretty=mir` text -> mir.json
// ---------------------------------------------------------------------------

pub fn split_top(s: &str) -> Vec<String> {
    let (mut parts, mut depth, mut cur, mut q) = (vec![], 0i32, String::new(), None::<char>);
    for ch in s.chars() {
        if let Some(qq) = q {
            cur.push(ch);
            if ch == qq { q = None; }
            continue;
        }
        match ch {
            '"' | '\'' => { q = Some(ch); cur.push(ch); }
            '<' | '(' | '[' => { depth += 1; cur.push(ch); }
            '>' | ')' | ']' => { depth -= 1; cur.push(ch); }
            ',' if depth == 0 => { parts.push(cur.trim().to_string()); cur = String::new(); }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts
}

pub fn base_local(place: &str) -> Option<String> {
    // Strip projections/wrappers (`(_4.0: T)`, `(_7 as Some).0`) down to the
    // base local, so SA places stay plain `_N`.
    let p = place.trim().trim_start_matches('(').trim();
    if p.starts_with('_') && p[1..].chars().take_while(|c| c.is_ascii_digit()).count() > 0 {
        let n: String = p[1..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if p.len() == 1 + n.len() {
            return Some(format!("_{}", n));
        }
        if p[1 + n.len()..].starts_with(|c| c == '.' || c == ':' || c == ' ') {
            return Some(format!("_{}", n));
        }
    }
    // fall back: leading _digits token
    let mut it = p.chars();
    if it.next() != Some('_') { return None; }
    let n: String = it.take_while(|c| c.is_ascii_digit()).collect();
    if n.is_empty() { None } else { Some(format!("_{}", n)) }
}

pub fn parse_operand(s: &str) -> Operand {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("move ") {
        return Operand::Move { place: base_local(rest).unwrap_or_else(|| rest.to_string()) };
    }
    if let Some(rest) = s.strip_prefix("copy ") {
        return Operand::Copy { place: base_local(rest).unwrap_or_else(|| rest.to_string()) };
    }
    if let Some(rest) = s.strip_prefix("const ") {
        return Operand::Const { value: rest.chars().take(60).collect(), str_bytes: None, str_len: None };
    }
    if s.starts_with('_') && base_local(s).as_deref() == Some(s) {
        return Operand::Copy { place: s.to_string() };
    }
    let v: String = s.chars().take(60).collect();
    Operand::Const { value: v, str_bytes: None, str_len: None }
}

pub fn sanitize_func(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    // strip ::<'...> and ::<...> turbofish
    loop {
        let start = match s.find("::<") {
            Some(i) => i,
            None => break,
        };
        let mut depth = 0;
        let mut end = None;
        for (j, ch) in s[start + 3..].char_indices() {
            match ch {
                '<' => depth += 1,
                '>' => { if depth == 0 { end = Some(start + 3 + j); break; } else { depth -= 1; } }
                _ => {}
            }
        }
        match end {
            Some(e) => { s.replace_range(start..=e, ""); }
            None => break,
        }
    }
    s = s.replace(" as ", "_as_").replace('&', "");
    // Collapse runs of separators into one `_` (same as Python
    // `re.sub(r"[^A-Za-z0-9_]+", "_", s)`), then trim edge `_`.
    let mut clean = String::new();
    let mut sep = true; // leading separators trimmed by collapsing
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            clean.push(c);
            sep = false;
        } else if !sep {
            clean.push('_');
            sep = true;
        }
    }
    while clean.ends_with('_') {
        clean.pop();
    }
    clean.chars().take(80).collect::<String>()
}

pub const BINOPS: &[&str] = &["Gt", "Eq", "Ne", "Lt", "Le", "Ge", "Add", "Sub", "Mul", "Div", "Rem",
    "BitAnd", "BitOr", "BitXor", "Shl", "Shr", "Offset"];

pub fn parse_call(line: &str) -> Option<(Option<String>, String, String, Vec<Operand>, Option<String>)> {
    // `DEST = FUNC(ARGS) -> [META];` ; terminator shapes excluded by caller
    let arrow = line.rfind("-> [")?;
    let (left, meta) = (&line[..arrow], &line[arrow + 4..]);
    let left = left.trim().strip_suffix(';').unwrap_or(left).trim();
    let (dest, call) = match left.find('=') {
        Some(i) => {
            let (d, c) = (&left[..i], &left[i + 1..]);
            (Some(d.trim().to_string()), c.trim())
        }
        None => (None, left),
    };
    // guard: not a drop/switchInt/assert/goto line (caller handles those first,
    // but keep the guard for safety)
    let head = call.split('(').next().unwrap_or("").trim();
    if ["drop", "switchInt", "assert", "goto"].contains(&head) {
        return None;
    }
    let paren = call.find('(')?;
    let func = call[..paren].trim().to_string();
    let args_str = call[paren + 1..].rsplit(')').nth(1).unwrap_or("").trim();
    // NOTE: rsplit(')') takes text before the LAST ')'; meta was already split
    // off, so this is the args region (may itself contain balanced parens,
    // which split_top respects).
    let args = if args_str.is_empty() { vec![] } else { split_top(args_str).into_iter().map(|a| parse_operand(&a)).collect() };
    let target = meta.split(',').find_map(|kv| {
        let kv = kv.trim().trim_end_matches(|c| c == ']' || c == ';');
        kv.strip_prefix("return:").map(|v| v.trim().to_string())
    });
    Some((dest, sanitize_func(&func), func, args, target))
}

pub fn parse_rvalue(rhs: &str) -> Rvalue {
    let rhs = rhs.trim();
    if let Some(rest) = rhs.strip_prefix("&mut ") {
        let p = rest.trim();
        return Rvalue::Ref { place: base_local(p).unwrap_or_else(|| p.to_string()), mut_: true, via: None, zst: false };
    }
    if let Some(rest) = rhs.strip_prefix('&') {
        let p = rest.trim();
        return Rvalue::Ref { place: base_local(p).unwrap_or_else(|| p.to_string()), mut_: false, via: None, zst: false };
    }
    // Cast before plain Use so `copy X as T (Kind)` is not swallowed.
    if let Some(as_pos) = rhs.find(" as ") {
        let tail = &rhs[as_pos + 4..];
        if let Some(kp) = tail.rfind('(') {
            if tail.ends_with(')') && kp > 0 && tail[..kp].trim().len() > 0
                && tail[kp + 1..tail.len() - 1].chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                let ty = tail[..kp].trim();
                return Rvalue::Cast {
                    op: Box::new(parse_operand(rhs[..as_pos].trim())),
                    ty: ty.chars().take(60).collect(),
                    castkind: None,
                    src_ty: None,
                };
            }
        }
    }
    if rhs.starts_with("move ") || rhs.starts_with("copy ") || rhs.starts_with("const ")
        || (rhs.starts_with('_') && base_local(rhs).as_deref() == Some(rhs))
    {
        return Rvalue::Use { op: parse_operand(rhs) };
    }
    if let Some(inner) = rhs.strip_prefix("no_retag copy ") {
        let inner = inner.trim();
        return Rvalue::Use { op: Operand::Copy { place: base_local(inner).unwrap_or_else(|| inner.chars().take(40).collect()) } };
    }
    if rhs.ends_with(')') {
        if let Some(pi) = rhs.find('(') {
            let name = &rhs[..pi];
            let inner = &rhs[pi + 1..rhs.len() - 1];
            if BINOPS.contains(&name) {
                let parts = split_top(inner);
                if parts.len() == 2 {
                    return Rvalue::BinOp { op: name.to_string(), left: Box::new(parse_operand(&parts[0])), right: Box::new(parse_operand(&parts[1])) };
                }
            }
            if name == "Not" {
                return Rvalue::UnOp { op: "Not".to_string(), operand: Box::new(parse_operand(inner)) };
            }
        }
    }
    if (rhs.starts_with('[') && rhs.ends_with(']')) || (rhs.starts_with('(') && rhs.ends_with(')')) {
        let inner = rhs[1..rhs.len() - 1].trim().trim_end_matches(',').trim();
        if !inner.is_empty() {
            return Rvalue::Aggregate { elems: split_top(inner).into_iter().map(|e| parse_operand(&e)).collect(), layout: None };
        }
    }
    Rvalue::Unsupported { text: rhs.chars().take(120).collect() }
}

pub fn sanitize_dest(d: &str) -> String {
    let d = d.trim();
    if d.starts_with('_') && base_local(d).as_deref() == Some(d) {
        return d.to_string();
    }
    base_local(d).unwrap_or_else(|| "_proj".to_string())
}

pub fn find_id(s: &str, key: &str) -> Option<String> {
    // find `key: bbN` inside bracket meta
    for kv in s.split(',') {
        let kv = kv.trim().trim_end_matches(|c| c == ']' || c == ';');
        if let Some(v) = kv.strip_prefix(key) {
            let v = v.trim().trim_start_matches(':').trim();
            if v.starts_with("bb") {
                return Some(v.chars().take_while(|c| c.is_alphanumeric()).collect());
            }
        }
    }
    None
}

pub fn parse_block(lines: &[String]) -> (Vec<Stmt>, Term, usize) {
    let (mut stmts, mut term, mut unsup) = (vec![], None::<Term>, 0);
    for ln in lines {
        let s = ln.trim();
        if s.is_empty() {
            continue;
        }
        if s.starts_with("drop(") && s.contains("-> [") {
            if let Some(arrow) = s.find("-> [") {
                let place = s["drop(".len()..].split(')').next().unwrap_or("").trim();
                let meta = &s[arrow + "-> [".len()..];
                let tgt = find_id(meta, "return").unwrap_or_else(|| "bb_unknown".to_string());
                term = Some(Term::Drop { place: base_local(place).unwrap_or_else(|| place.to_string()), target: tgt });
                continue;
            }
        }
        if s.starts_with("switchInt(") && s.contains("-> [") {
            if let Some(arrow) = s.rfind(") -> [") {
                let discr = parse_operand(s["switchInt(".len()..arrow].trim());
                let meta = &s[arrow + ") -> [".len()..];
                let mut targets = vec![];
                for kv in meta.split(',') {
                    let kv = kv.trim().trim_end_matches(|c| c == ']' || c == ';');
                    if kv.starts_with("otherwise:") {
                        continue;
                    }
                    if let Some(ci) = kv.find(':') {
                        let (v, t) = (kv[..ci].trim(), kv[ci + 1..].trim());
                        if t.starts_with("bb") {
                            targets.push((v.to_string(), t.chars().take_while(|c| c.is_alphanumeric()).collect()));
                        }
                    }
                }
                let ow = find_id(meta, "otherwise").unwrap_or_else(|| "bb_unknown".to_string());
                term = Some(Term::SwitchInt { discr: Box::new(discr), targets, otherwise: ow });
                continue;
            }
        }
        if s.starts_with("assert(") && s.contains("-> [") {
            if let Some(arrow) = s.rfind(") -> [") {
                let inner = &s["assert(".len()..arrow];
                let first = split_top(inner).into_iter().next().unwrap_or_default();
                let meta = &s[arrow + ") -> [".len()..];
                let tgt = find_id(meta, "success").unwrap_or_else(|| "bb_unknown".to_string());
                term = Some(Term::Assert { cond: Box::new(parse_operand(&first)), target: tgt, msg: Some(inner.chars().take(80).collect()), expected: None });
                continue;
            }
        }
        if s.starts_with("goto") {
            if let Some(t) = s.split("->").nth(1) {
                let t: String = t.trim().trim_end_matches(';').chars().take_while(|c| c.is_alphanumeric()).collect();
                term = Some(Term::Goto { target: t });
                continue;
            }
        }
        if s == "return;" || s == "return" {
            term = Some(Term::Return);
            continue;
        }
        if s == "resume;" || s == "resume" {
            term = Some(Term::Resume);
            continue;
        }
        if s.contains("-> [") {
            if let Some((dest, func, raw, args, target)) = parse_call(s) {
                match dest {
                    Some(d) => {
                        term = Some(Term::Call {
                            func,
                            func_raw: Some(raw.chars().take(100).collect()),
                            args,
                            dest: Some(d),
                            target,
                            sig: None,
                        });
                    }
                    None => {
                        stmts.push(Stmt::Assign {
                            dest: "_0".to_string(),
                            dest_place: Some("_0".to_string()),
                            rvalue: Rvalue::Call { func, args, sig: None },
                        });
                        term = Some(Term::Unreachable);
                    }
                }
                continue;
            }
        }
        if let Some(eq) = s.find(" = ") {
            let (d, r) = (s[..eq].trim(), s[eq + 3..].trim().trim_end_matches(';').trim());
            // skip `let` declarations (no leading _local dest)
            if d.starts_with("let ") {
                continue;
            }
            let rv = parse_rvalue(r);
            if matches!(rv, Rvalue::Unsupported { .. }) {
                unsup += 1;
            }
            let dest = sanitize_dest(d);
            stmts.push(Stmt::Assign { dest_place: Some(d.chars().take(80).collect()), dest, rvalue: rv });
            continue;
        }
        unsup += 1;
        stmts.push(Stmt::UnsupportedStmt { text: s.chars().take(120).collect() });
    }
    (stmts, term.unwrap_or(Term::Return), unsup)
}

pub fn extract_fn(text: &str, fname: &str) -> Option<Vec<String>> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.iter().position(|l| {
        l.starts_with("fn ") && l["fn ".len()..].split('(').next().unwrap_or("") == fname
    })?;
    let mut end = lines.len();
    for i in start + 1..lines.len() {
        let l = lines[i];
        if l.starts_with("fn ") || (l.starts_with("alloc") && l.contains('(')) {
            end = i;
            break;
        }
    }
    while end > start + 1 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some(lines[start..end].iter().map(|s| s.to_string()).collect())
}

pub fn is_bb_header(ln: &str) -> Option<String> {
    let t = ln.trim();
    if !t.starts_with("bb") {
        return None;
    }
    let rest = &t[2..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let after = rest[digits.len()..].trim();
    let after = after.strip_prefix("(cleanup)").unwrap_or(after).trim();
    if after.strip_prefix(':').map(|s| s.trim().strip_prefix('{').is_some()).unwrap_or(false) {
        return Some(format!("bb{}", digits));
    }
    // also accept `bb0: {` with trailing content? headers are exactly `bbN: {`
    if after == ": {" || after == ":" {
        return Some(format!("bb{}", digits));
    }
    None
}

pub fn stmt_kind(s: &Stmt) -> String {
    match s {
        Stmt::Assign { rvalue, .. } => format!("Assign/{}", rvalue_kind(rvalue)),
        Stmt::StorageLive { .. } => "StorageLive".to_string(),
        Stmt::StorageDead { .. } => "StorageDead".to_string(),
        Stmt::Nop { .. } => "Nop".to_string(),
        Stmt::SetDisc { .. } => "SetDisc".to_string(),
        Stmt::UnsupportedStmt { .. } => "UnsupportedStmt".to_string(),
    }
}

pub fn rvalue_kind(rv: &Rvalue) -> String {
    match rv {
        Rvalue::Use { .. } => "Use".to_string(),
        Rvalue::Ref { .. } => "Ref".to_string(),
        Rvalue::Call { .. } => "Call".to_string(),
        Rvalue::BinOp { op, .. } => format!("BinOp/{}", op),
        Rvalue::UnOp { op, .. } => format!("UnOp/{}", op),
        Rvalue::Cast { .. } => "Cast".to_string(),
        Rvalue::Aggregate { .. } => "Aggregate".to_string(),
        Rvalue::Discriminant { .. } => "Discriminant".to_string(),
        Rvalue::RawPtr { .. } => "RawPtr".to_string(),
        Rvalue::Repeat { .. } => "Repeat".to_string(),
        Rvalue::ThreadLocal { .. } => "ThreadLocal".to_string(),
        Rvalue::Unsupported { .. } => "Unsupported".to_string(),
    }
}

pub fn term_kind(t: &Term) -> &'static str {
    match t {
        Term::Goto { .. } => "Goto",
        Term::Return => "Return",
        Term::Resume => "Resume",
        Term::Call { .. } => "Call",
        Term::Drop { .. } => "Drop",
        Term::SwitchInt { .. } => "SwitchInt",
        Term::Assert { .. } => "Assert",
        Term::Unreachable => "Unreachable",
        Term::InlineAsm { .. } => "InlineAsm",
        Term::Unsupported { .. } => "T/Unsupported",
    }
}
