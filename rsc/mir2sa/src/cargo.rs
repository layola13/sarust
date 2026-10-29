//! cargo: Cargo.toml subset parser + feature resolution + JSON build plan.
//!
//! Stream 7 (sarust Phase 1, first half). Stream 8 (module tree + multi-crate
//! build orchestration) consumes the JSON plan emitted by `mir2sa cargo`
//! (see `docs/cargo-plan.md`); it does NOT need to link this module.
//!
//! Design (Jev jev-1.14.0, confidence 1.0 parser / 0.76 interface):
//! hand-rolled TOML subset parser, zero new dependencies (the container is
//! effectively offline and the single-file pipeline must not regress), and a
//! documented JSON plan on the CLI. Anything outside the subset is recorded
//! LOUD in `warnings` / `unsupported`, never silently dropped. The one hard
//! error is an unknown `--features` entry: proceeding would build the wrong
//! cfg set, so the plan refuses to exist instead of lying.
//!
//! Supported subset (Cargo.toml):
//!   [package] name (required) / version / edition / autobins / autolib
//!   [lib] name / path / crate-type (explicit only)
//!   [[bin]] name / path (default src/main.rs; src/bin/<name>.rs for [[bin]])
//!   [dependencies] / [dev-dependencies] / [build-dependencies] and
//!   [target.<cfg-expr-or-triple>.{dependencies,dev-dependencies,
//!   build-dependencies}] — string form, inline-table form, and table form
//!   ([dependencies.foo]) all parsed; `path` deps are resolved recursively,
//!   `optional` / `default-features` / `features` / `package` (rename) kept.
//!   [features] name = ["dep:other", "other/feat", "plain-feat"]
//!   [profile.<name>] keys (values stringified)
//!   [workspace] members are recorded but NOT expanded (stream 8 owns
//!   workspaces); a `dep.workspace = true` entry is loud-unsupported.
//! NOT resolved (loud, in plan.unsupported): crates.io downloads + version
//! resolution, git dependencies, [patch]/[replace]. A plan with unsupported
//! entries still exits 0 — it is a partial plan, and the list says exactly
//! what is missing. Dev dependencies are parsed but never built by a plan
//! (test/example builds are stream 8's call).

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------- TOML value
#[derive(Debug, Clone, PartialEq)]
enum Val {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Arr(Vec<Val>),
    Table(Vec<(String, Val)>),
}

impl Val {
    fn as_str(&self) -> Option<&str> {
        match self {
            Val::Str(s) => Some(s),
            _ => None,
        }
    }
    fn stringified(&self) -> String {
        match self {
            Val::Str(s) => s.clone(),
            Val::Int(i) => i.to_string(),
            Val::Float(f) => f.to_string(),
            Val::Bool(b) => b.to_string(),
            Val::Arr(v) => v
                .iter()
                .map(|x| x.stringified())
                .collect::<Vec<_>>()
                .join(", "),
            Val::Table(t) => t
                .iter()
                .map(|(k, v)| format!("{} = {}", k, v.stringified()))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }
}

// ------------------------------------------------------------- TOML document
type Map = BTreeMap<String, Node>;

#[derive(Debug, Clone)]
enum Node {
    Val(Val),
    Table(Map),
    ArrTable(Vec<Map>),
}

struct Parser {
    chars: Vec<char>,
    i: usize,
}

impl Parser {
    fn new(text: &str) -> Self {
        Parser {
            chars: text.chars().collect(),
            i: 0,
        }
    }
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }
    fn peek_at(&self, off: usize) -> Option<char> {
        self.chars.get(self.i + off).copied()
    }
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.i += 1;
        }
        c
    }
    fn starts_with(&self, s: &str) -> bool {
        s.chars()
            .enumerate()
            .all(|(k, c)| self.peek_at(k) == Some(c))
    }
    fn err(&self, msg: &str) -> String {
        let line = self.chars[..self.i.min(self.chars.len())]
            .iter()
            .filter(|&&c| c == '\n')
            .count()
            + 1;
        format!("TOML parse error near line {}: {}", line, msg)
    }
    fn skip_inline_ws(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t')) {
            self.i += 1;
        }
    }
    fn skip_ws_comments(&mut self) {
        loop {
            match self.peek() {
                Some(' ') | Some('\t') | Some('\n') | Some('\r') => self.i += 1,
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }
    fn parse_key_part(&mut self) -> Result<String, String> {
        self.skip_inline_ws();
        match self.peek() {
            Some('"') => self.parse_basic_string(),
            Some('\'') => self.parse_literal_string(),
            Some(c) if c.is_alphanumeric() || c == '_' || c == '-' => {
                let mut s = String::new();
                while let Some(c) = self.peek() {
                    if c.is_alphanumeric() || c == '_' || c == '-' {
                        s.push(c);
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                Ok(s)
            }
            _ => Err(self.err("expected a key")),
        }
    }
    fn parse_key(&mut self) -> Result<Vec<String>, String> {
        let mut parts = vec![self.parse_key_part()?];
        loop {
            self.skip_inline_ws();
            if self.peek() == Some('.') {
                self.i += 1;
                parts.push(self.parse_key_part()?);
            } else {
                break;
            }
        }
        Ok(parts)
    }
    fn hex_val(c: char) -> Option<u32> {
        c.to_digit(16)
    }
    fn parse_escape(&mut self) -> Result<char, String> {
        // self.i is just past the backslash.
        match self.bump() {
            Some('n') => Ok('\n'),
            Some('t') => Ok('\t'),
            Some('r') => Ok('\r'),
            Some('"') => Ok('"'),
            Some('\\') => Ok('\\'),
            Some('e') => Ok('\u{1b}'),
            Some('u') | Some('U') => {
                let n = if self.chars.get(self.i - 1) == Some(&'u') {
                    4
                } else {
                    8
                };
                let mut v: u32 = 0;
                for _ in 0..n {
                    match self.bump().and_then(Self::hex_val) {
                        Some(d) => v = v * 16 + d,
                        None => return Err(self.err("bad \\u escape")),
                    }
                }
                char::from_u32(v).ok_or_else(|| self.err("bad unicode scalar"))
            }
            _ => Err(self.err("unknown string escape")),
        }
    }
    fn parse_basic_string(&mut self) -> Result<String, String> {
        // Consume opening quote; handle """ multiline form.
        self.i += 1;
        let multiline = self.starts_with("\"\"");
        if multiline {
            self.i += 2;
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some('"') => {
                    if multiline {
                        if self.peek() == Some('"') && self.peek_at(1) == Some('"') {
                            self.i += 2;
                            return Ok(out);
                        }
                        out.push('"');
                    } else {
                        return Ok(out);
                    }
                }
                Some('\\') => out.push(self.parse_escape()?),
                Some(c) => out.push(c),
            }
        }
    }
    fn parse_literal_string(&mut self) -> Result<String, String> {
        self.i += 1;
        let multiline = self.starts_with("''");
        if multiline {
            self.i += 2;
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated literal string")),
                Some('\'') => {
                    if multiline {
                        if self.peek() == Some('\'') && self.peek_at(1) == Some('\'') {
                            self.i += 2;
                            return Ok(out);
                        }
                        out.push('\'');
                    } else {
                        return Ok(out);
                    }
                }
                Some(c) => out.push(c),
            }
        }
    }
    fn parse_value(&mut self) -> Result<Val, String> {
        self.skip_inline_ws();
        match self.peek() {
            Some('"') => Ok(Val::Str(self.parse_basic_string()?)),
            Some('\'') => Ok(Val::Str(self.parse_literal_string()?)),
            Some('[') => {
                self.i += 1;
                let mut items = vec![];
                loop {
                    self.skip_ws_comments();
                    if self.peek() == Some(']') {
                        self.i += 1;
                        return Ok(Val::Arr(items));
                    }
                    if self.peek().is_none() {
                        return Err(self.err("unterminated array"));
                    }
                    items.push(self.parse_value()?);
                    self.skip_ws_comments();
                    match self.peek() {
                        Some(',') => {
                            self.i += 1;
                        }
                        Some(']') => {}
                        _ => return Err(self.err("expected ',' or ']' in array")),
                    }
                }
            }
            Some('{') => {
                self.i += 1;
                let mut entries = vec![];
                loop {
                    self.skip_ws_comments();
                    if self.peek() == Some('}') {
                        self.i += 1;
                        return Ok(Val::Table(entries));
                    }
                    if self.peek().is_none() {
                        return Err(self.err("unterminated inline table"));
                    }
                    let key = self.parse_key()?;
                    self.skip_inline_ws();
                    if self.bump() != Some('=') {
                        return Err(self.err("expected '=' in inline table"));
                    }
                    let val = self.parse_value()?;
                    entries.push((key.join("."), val));
                    self.skip_ws_comments();
                    match self.peek() {
                        Some(',') => {
                            self.i += 1;
                        }
                        Some('}') => {}
                        _ => return Err(self.err("expected ',' or '}' in inline table")),
                    }
                }
            }
            Some(_) => {
                // Bare token: bool / int / float / datetime (kept as string).
                let mut tok = String::new();
                while let Some(c) = self.peek() {
                    if c == ',' || c == ']' || c == '}' || c == '#' || c == '\n' || c == '\r' {
                        break;
                    }
                    tok.push(c);
                    self.i += 1;
                }
                let tok = tok.trim().to_string();
                if tok == "true" {
                    return Ok(Val::Bool(true));
                }
                if tok == "false" {
                    return Ok(Val::Bool(false));
                }
                let cleaned: String = tok.chars().filter(|&c| c != '_').collect();
                if let Some(hex) = cleaned
                    .strip_prefix("0x")
                    .or_else(|| cleaned.strip_prefix("0X"))
                {
                    if let Ok(v) = i64::from_str_radix(hex, 16) {
                        return Ok(Val::Int(v));
                    }
                }
                if let Ok(v) = cleaned.parse::<i64>() {
                    return Ok(Val::Int(v));
                }
                if let Ok(v) = cleaned.parse::<f64>() {
                    return Ok(Val::Float(v));
                }
                if tok.is_empty() {
                    return Err(self.err("expected a value"));
                }
                Ok(Val::Str(tok))
            }
            None => Err(self.err("expected a value, found end of input")),
        }
    }
    fn parse_doc(&mut self) -> Result<Map, String> {
        let mut root: Map = BTreeMap::new();
        let mut cur_path: Vec<String> = vec![];
        let mut cur_is_array = false;
        loop {
            self.skip_ws_comments();
            match self.peek() {
                None => break,
                Some('[') => {
                    self.i += 1;
                    let is_array = self.peek() == Some('[');
                    if is_array {
                        self.i += 1;
                    }
                    let path = self.parse_key()?;
                    self.skip_inline_ws();
                    if is_array {
                        if self.bump() != Some(']') || self.bump() != Some(']') {
                            return Err(self.err("expected ']]'"));
                        }
                    } else if self.bump() != Some(']') {
                        return Err(self.err("expected ']'"));
                    }
                    // Create / extend the header table.
                    ensure_table(&mut root, &path, is_array)?;
                    cur_path = path;
                    cur_is_array = is_array;
                }
                Some(_) => {
                    let key = self.parse_key()?;
                    self.skip_inline_ws();
                    if self.bump() != Some('=') {
                        return Err(self.err("expected '='"));
                    }
                    let val = self.parse_value()?;
                    let mut full = cur_path.clone();
                    full.extend(key);
                    set_value(&mut root, &full, val, cur_is_array)?;
                }
            }
            // Rest of the line must be comment / newline only.
            self.skip_inline_ws();
            match self.peek() {
                Some('#') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.i += 1;
                    }
                }
                Some('\n') | Some('\r') | None => {}
                Some(c) => return Err(self.err(&format!("unexpected trailing character '{}'", c))),
            }
        }
        Ok(root)
    }
}

/// Navigate `path` (all but the last segment must be tables, or the last
/// element of an array-of-tables when the current header is one) and set the
/// final key to `val`. Dotted keys create intermediate tables.
fn set_value(root: &mut Map, path: &[String], val: Val, _cur_is_array: bool) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty key path".to_string());
    }
    let mut cur = root;
    for seg in &path[..path.len() - 1] {
        let entry = cur
            .entry(seg.clone())
            .or_insert_with(|| Node::Table(BTreeMap::new()));
        match entry {
            Node::Table(m) => cur = m,
            Node::ArrTable(v) => {
                if v.is_empty() {
                    v.push(BTreeMap::new());
                }
                cur = v.last_mut().unwrap();
            }
            Node::Val(_) => return Err(format!("key '{}' conflicts with a value", seg)),
        }
    }
    let last = &path[path.len() - 1];
    if cur.contains_key(last) {
        return Err(format!("duplicate key '{}'", last));
    }
    cur.insert(last.clone(), Node::Val(val));
    Ok(())
}

fn ensure_table(root: &mut Map, path: &[String], is_array: bool) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty table header".to_string());
    }
    let mut cur = root;
    for seg in &path[..path.len() - 1] {
        let entry = cur
            .entry(seg.clone())
            .or_insert_with(|| Node::Table(BTreeMap::new()));
        match entry {
            Node::Table(m) => cur = m,
            Node::ArrTable(v) => {
                if v.is_empty() {
                    v.push(BTreeMap::new());
                }
                cur = v.last_mut().unwrap();
            }
            Node::Val(_) => return Err(format!("table '{}' conflicts with a value", seg)),
        }
    }
    let last = &path[path.len() - 1];
    match cur.get_mut(last) {
        None => {
            if is_array {
                cur.insert(last.clone(), Node::ArrTable(vec![BTreeMap::new()]));
            } else {
                cur.insert(last.clone(), Node::Table(BTreeMap::new()));
            }
        }
        Some(Node::ArrTable(v)) if is_array => {
            v.push(BTreeMap::new());
        }
        Some(Node::Table(_)) if !is_array => {}
        Some(_) => return Err(format!("table '{}' redefined with a different shape", last)),
    }
    Ok(())
}

fn get_table<'a>(map: &'a Map, key: &str) -> Option<&'a Map> {
    match map.get(key) {
        Some(Node::Table(m)) => Some(m),
        _ => None,
    }
}

fn str_field(map: &Map, key: &str) -> Option<String> {
    match map.get(key) {
        Some(Node::Val(Val::Str(s))) => Some(s.clone()),
        _ => None,
    }
}

fn bool_field(map: &Map, key: &str) -> Option<bool> {
    match map.get(key) {
        Some(Node::Val(Val::Bool(b))) => Some(*b),
        _ => None,
    }
}

fn arr_str_field(map: &Map, key: &str) -> Option<Vec<String>> {
    match map.get(key) {
        Some(Node::Val(Val::Arr(items))) => {
            let mut out = vec![];
            for it in items {
                out.push(it.as_str()?.to_string());
            }
            Some(out)
        }
        _ => None,
    }
}

// ------------------------------------------------------------ data structures
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CargoTarget {
    pub name: String,
    /// Path as resolved against the manifest dir (absolute in plans).
    pub path: String,
    pub crate_type: String,
    /// Explicit `crate-type` from the manifest, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explicit_crate_type: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Dependency {
    /// Name as written in the manifest key (after rename handling, see below).
    pub name: String,
    /// Real package name when the manifest renames it (`foo = { package = "bar" }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Path exactly as written in the manifest (relative to the manifest dir).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Absolute dir of a resolved path dependency (filled in by plan building).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    pub optional: bool,
    pub default_features: bool,
    pub features: Vec<String>,
    pub kind: String,
    /// `normal` / `dev` / `build`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// True when the entry is `dep.workspace = true` (inherited, unresolved).
    pub workspace_inherited: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CargoManifest {
    pub manifest_path: String,
    pub manifest_dir: String,
    pub package_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lib: Option<CargoTarget>,
    pub bins: Vec<CargoTarget>,
    pub dependencies: Vec<Dependency>,
    pub features: BTreeMap<String, Vec<String>>,
    pub profiles: BTreeMap<String, BTreeMap<String, String>>,
    pub workspace_members: Vec<String>,
    /// Parse-level loud items (subset gaps, unknown keys, workspace notes).
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PlannedCrate {
    pub name: String,
    pub manifest_dir: String,
    pub manifest_path: String,
    pub edition: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lib: Option<CargoTarget>,
    pub bins: Vec<CargoTarget>,
    pub enabled_features: Vec<String>,
    /// argv tokens to splice into a sars invocation, e.g.
    /// `["--cfg", "feature=\"fast\""]`. sars forwards non-`--rsc-out`
    /// arguments to rustc, so these turn into real `cfg(feature)` gates.
    pub cfg_args: Vec<String>,
    /// `root` for the plan root, else the edge kind from its parent.
    pub edge_kind: String,
    /// This crate's own parsed dependencies (path deps carry resolved_path).
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BuildPlan {
    pub schema_version: u32,
    pub root_manifest: String,
    pub root_crate: String,
    /// Dependency-first order; the root crate is LAST.
    pub crates: Vec<PlannedCrate>,
    /// Convenience copy of the root crate's resolved features.
    pub enabled_features: Vec<String>,
    /// Convenience copy of the root crate's cfg argv tokens.
    pub cfg_args: Vec<String>,
    /// Build-affecting constructs this plan does NOT resolve (crates.io
    /// downloads, git deps, workspace inheritance, missing path dirs).
    pub unsupported: Vec<String>,
    /// Parse-level warnings carried over from every parsed manifest.
    pub warnings: Vec<String>,
    /// Path deps excluded because their target cfg does not match this host.
    pub excluded_target_deps: Vec<String>,
}

// ------------------------------------------------------------- path utilities
fn normalize_path(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn absolutize(p: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(p) {
        return c;
    }
    if p.is_absolute() {
        normalize_path(p)
    } else {
        match std::env::current_dir() {
            Ok(cwd) => normalize_path(&cwd.join(p)),
            Err(_) => normalize_path(p),
        }
    }
}

// ------------------------------------------------------------ manifest parsing
fn dep_from_node(
    name: &str,
    node: &Node,
    kind: &str,
    target: Option<String>,
    warnings: &mut Vec<String>,
) -> Dependency {
    let mut dep = Dependency {
        name: name.to_string(),
        package: None,
        version: None,
        path: None,
        resolved_path: None,
        git: None,
        git_ref: None,
        optional: false,
        default_features: true,
        features: vec![],
        kind: kind.to_string(),
        target,
        workspace_inherited: false,
    };
    // Inline-table form (`foo = { path = ... }`) arrives as
    // Node::Val(Val::Table(..)); section form (`[dependencies.foo]`) arrives
    // as Node::Table(..). Normalize both to one Map view.
    let inline_map: Map = match node {
        Node::Val(Val::Table(entries)) => entries
            .iter()
            .map(|(k, v)| (k.clone(), Node::Val(v.clone())))
            .collect(),
        _ => BTreeMap::new(),
    };
    let table: Option<&Map> = match node {
        Node::Table(m) => Some(m),
        Node::Val(Val::Table(_)) => Some(&inline_map),
        _ => None,
    };
    match table {
        Some(m) => {
            dep.version = str_field(m, "version");
            dep.path = str_field(m, "path");
            dep.git = str_field(m, "git");
            if let Some(b) = str_field(m, "branch") {
                dep.git_ref = Some(format!("branch {}", b));
            } else if let Some(t) = str_field(m, "tag") {
                dep.git_ref = Some(format!("tag {}", t));
            } else if let Some(r) = str_field(m, "rev") {
                dep.git_ref = Some(format!("rev {}", r));
            }
            dep.package = str_field(m, "package");
            dep.optional = bool_field(m, "optional").unwrap_or(false);
            dep.default_features = bool_field(m, "default-features").unwrap_or(true);
            dep.features = arr_str_field(m, "features").unwrap_or_default();
            dep.workspace_inherited = bool_field(m, "workspace").unwrap_or(false);
            for key in m.keys() {
                if !matches!(
                    key.as_str(),
                    "version"
                        | "path"
                        | "git"
                        | "branch"
                        | "tag"
                        | "rev"
                        | "package"
                        | "optional"
                        | "default-features"
                        | "features"
                        | "workspace"
                ) {
                    warnings.push(format!(
                        "dependency '{}' has unknown key '{}' (ignored, loud)",
                        name, key
                    ));
                }
            }
        }
        None => match node {
            Node::Val(Val::Str(v)) => dep.version = Some(v.clone()),
            Node::Val(_) => {
                warnings.push(format!("dependency '{}' has a non-string, non-table value; kept with no version (loud)", name));
            }
            Node::ArrTable(_) => {
                warnings.push(format!("dependency '{}' is an array-of-tables, which Cargo does not allow; kept with no version (loud)", name));
            }
            Node::Table(_) => unreachable!(),
        },
    }
    dep
}

fn deps_from_section(
    map: &Map,
    kind: &str,
    target: Option<String>,
    out: &mut Vec<Dependency>,
    warnings: &mut Vec<String>,
) {
    for (name, node) in map {
        out.push(dep_from_node(name, node, kind, target.clone(), warnings));
    }
}

/// Parse one manifest file. `manifest_path` may be a dir (Cargo.toml is
/// appended) or a file. Returns Err only for fatal problems (unreadable file,
/// TOML syntax error, missing [package] name); subset gaps are warnings.
pub fn parse_manifest(input: &Path) -> Result<CargoManifest, String> {
    let manifest_path = if input.is_dir() {
        input.join("Cargo.toml")
    } else {
        input.to_path_buf()
    };
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read {}: {}", manifest_path.display(), e))?;
    let mut parser = Parser::new(&text);
    let doc = parser.parse_doc()?;
    let abs_manifest = absolutize(&manifest_path);
    let manifest_dir = abs_manifest
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let mut warnings: Vec<String> = vec![];

    let package = get_table(&doc, "package")
        .ok_or_else(|| format!("{}: missing [package]", manifest_path.display()))?;
    let package_name = str_field(package, "name")
        .ok_or_else(|| format!("{}: [package] has no name", manifest_path.display()))?;
    let package_version = str_field(package, "version");
    let edition = str_field(package, "edition");
    let autobins = bool_field(package, "autobins").unwrap_or(true);
    let autolib = bool_field(package, "autolib").unwrap_or(true);

    // Targets.
    let lib_name_default = package_name.replace('-', "_");
    let mut lib: Option<CargoTarget> = None;
    if let Some(lib_map) = get_table(&doc, "lib") {
        let name = str_field(lib_map, "name").unwrap_or_else(|| lib_name_default.clone());
        let path = str_field(lib_map, "path").unwrap_or_else(|| "src/lib.rs".to_string());
        let explicit = arr_str_field(lib_map, "crate-type");
        lib = Some(CargoTarget {
            name,
            path: absolutize(&manifest_dir.join(&path))
                .to_string_lossy()
                .to_string(),
            crate_type: "lib".to_string(),
            explicit_crate_type: explicit,
        });
    } else if autolib && manifest_dir.join("src/lib.rs").is_file() {
        lib = Some(CargoTarget {
            name: lib_name_default.clone(),
            path: absolutize(&manifest_dir.join("src/lib.rs"))
                .to_string_lossy()
                .to_string(),
            crate_type: "lib".to_string(),
            explicit_crate_type: None,
        });
    }
    let mut bins: Vec<CargoTarget> = vec![];
    if let Some(Node::ArrTable(arr)) = doc.get("bin") {
        for entry in arr {
            match str_field(entry, "name") {
                Some(name) => {
                    let path =
                        str_field(entry, "path").unwrap_or_else(|| format!("src/bin/{}.rs", name));
                    bins.push(CargoTarget {
                        name,
                        path: absolutize(&manifest_dir.join(&path))
                            .to_string_lossy()
                            .to_string(),
                        crate_type: "bin".to_string(),
                        explicit_crate_type: arr_str_field(entry, "crate-type"),
                    });
                }
                None => warnings.push("[[bin]] entry without a name (skipped, loud)".to_string()),
            }
        }
    }
    if autobins
        && manifest_dir.join("src/main.rs").is_file()
        && !bins.iter().any(|b| b.name == package_name)
    {
        bins.push(CargoTarget {
            name: package_name.clone(),
            path: absolutize(&manifest_dir.join("src/main.rs"))
                .to_string_lossy()
                .to_string(),
            crate_type: "bin".to_string(),
            explicit_crate_type: None,
        });
    }

    // Dependencies: plain sections + target sections.
    let mut dependencies: Vec<Dependency> = vec![];
    for (section, kind) in [
        ("dependencies", "normal"),
        ("dev-dependencies", "dev"),
        ("build-dependencies", "build"),
    ] {
        if let Some(m) = get_table(&doc, section) {
            deps_from_section(m, kind, None, &mut dependencies, &mut warnings);
        }
        // Table form: [dependencies.foo] (a Table node under `dependencies`
        // would already be handled above as a dep entry; this arm is for the
        // [target.*.dependencies] shape handled below).
        let _ = kind;
    }
    if let Some(target_map) = get_table(&doc, "target") {
        for (target_key, target_node) in target_map {
            if let Node::Table(tmap) = target_node {
                for (section, kind) in [
                    ("dependencies", "normal"),
                    ("dev-dependencies", "dev"),
                    ("build-dependencies", "build"),
                ] {
                    if let Some(m) = get_table(tmap, section) {
                        deps_from_section(
                            m,
                            kind,
                            Some(target_key.clone()),
                            &mut dependencies,
                            &mut warnings,
                        );
                    }
                }
            }
        }
    }

    // Features.
    let mut features: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if let Some(fmap) = get_table(&doc, "features") {
        for (name, node) in fmap {
            match node {
                Node::Val(Val::Arr(items)) => {
                    let mut vals = vec![];
                    let mut bad = false;
                    for it in items {
                        match it.as_str() {
                            Some(s) => vals.push(s.to_string()),
                            None => bad = true,
                        }
                    }
                    if bad {
                        warnings.push(format!(
                            "feature '{}' has a non-string item (dropped, loud)",
                            name
                        ));
                    }
                    features.insert(name.clone(), vals);
                }
                Node::Val(Val::Str(s)) => {
                    warnings.push(format!(
                        "feature '{}' is a bare string, not an array (wrapped, loud)",
                        name
                    ));
                    features.insert(name.clone(), vec![s.clone()]);
                }
                _ => warnings.push(format!(
                    "feature '{}' is not an array (dropped, loud)",
                    name
                )),
            }
        }
    }

    // Profiles.
    let mut profiles: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    if let Some(pmap) = get_table(&doc, "profile") {
        for (name, node) in pmap {
            if let Node::Table(m) = node {
                let mut flat = BTreeMap::new();
                for (k, v) in m {
                    match v {
                        Node::Val(val) => {
                            flat.insert(k.clone(), val.stringified());
                        }
                        _ => warnings.push(format!(
                            "profile '{}' key '{}' is a table (dropped, loud)",
                            name, k
                        )),
                    }
                }
                profiles.insert(name.clone(), flat);
            }
        }
    }

    // Workspace + patch/replace: recorded, not expanded (stream 8).
    let mut workspace_members: Vec<String> = vec![];
    if let Some(ws) = get_table(&doc, "workspace") {
        if let Some(members) = arr_str_field(ws, "members") {
            workspace_members = members;
        }
        if !workspace_members.is_empty() {
            warnings.push(format!(
                "workspace with {} member(s) is not expanded by stream 7; stream 8 owns workspaces (loud)",
                workspace_members.len()
            ));
        }
    }
    if doc.contains_key("patch") || doc.contains_key("replace") {
        warnings.push("[patch]/[replace] sections are not applied by stream 7 (loud)".to_string());
    }

    Ok(CargoManifest {
        manifest_path: abs_manifest.to_string_lossy().to_string(),
        manifest_dir: manifest_dir.to_string_lossy().to_string(),
        package_name,
        package_version,
        edition,
        lib,
        bins,
        dependencies,
        features,
        profiles,
        workspace_members,
        warnings,
    })
}

// ------------------------------------------------------------ feature options
#[derive(Debug, Clone, Default)]
pub struct FeatureOpts {
    /// `--features` entries, already comma-split by the CLI.
    pub features: Vec<String>,
    pub no_default_features: bool,
    pub all_features: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedFeatures {
    pub enabled: BTreeSet<String>,
    /// Optional deps switched on (via `dep:` items, implicit features, or
    /// a `dep/feat` item naming an optional dep).
    pub enabled_optional_deps: BTreeSet<String>,
    /// feature requests for dependencies: dep name -> features of that dep.
    pub dep_features: BTreeMap<String, BTreeSet<String>>,
}

fn optional_dep_names(manifest: &CargoManifest) -> BTreeSet<String> {
    manifest
        .dependencies
        .iter()
        .filter(|d| d.optional)
        .map(|d| d.name.clone())
        .collect()
}

/// Resolve one crate's features. Err on unknown requested / referenced
/// feature names: a plan built from a wrong feature set would silently
/// miscompile `cfg(feature)` gates, which the loud rule forbids.
pub fn resolve_features(
    manifest: &CargoManifest,
    opts: &FeatureOpts,
) -> Result<ResolvedFeatures, String> {
    let mut out = ResolvedFeatures::default();
    let optional = optional_dep_names(manifest);
    let mut work: Vec<String> = vec![];
    let mut enable = |out: &mut ResolvedFeatures, work: &mut Vec<String>, f: &str| {
        if out.enabled.insert(f.to_string()) {
            work.push(f.to_string());
        }
    };
    if opts.all_features {
        for name in manifest.features.keys() {
            if name != "default" {
                enable(&mut out, &mut work, name);
            }
        }
        if !opts.no_default_features {
            enable(&mut out, &mut work, "default");
        }
    } else {
        if !opts.no_default_features && manifest.features.contains_key("default") {
            enable(&mut out, &mut work, "default");
        }
        for req in &opts.features {
            let req = req.trim();
            if req.is_empty() {
                continue;
            }
            if let Some(dep) = req.strip_prefix("dep:") {
                if !optional.contains(dep) {
                    return Err(format!(
                        "crate '{}': --features entry 'dep:{}' does not name an optional dependency",
                        manifest.package_name, dep
                    ));
                }
                out.enabled_optional_deps.insert(dep.to_string());
            } else if let Some((dep, feat)) = req.split_once('/') {
                if !manifest.dependencies.iter().any(|d| d.name == dep) {
                    return Err(format!(
                        "crate '{}': --features entry '{}' names unknown dependency '{}'",
                        manifest.package_name, req, dep
                    ));
                }
                out.dep_features
                    .entry(dep.to_string())
                    .or_default()
                    .insert(feat.to_string());
                if optional.contains(dep) {
                    out.enabled_optional_deps.insert(dep.to_string());
                }
            } else if manifest.features.contains_key(req) {
                enable(&mut out, &mut work, req);
            } else if optional.contains(req) {
                // Implicit feature for an optional dependency.
                out.enabled_optional_deps.insert(req.to_string());
            } else {
                return Err(format!(
                    "crate '{}': unknown feature '{}' (known: {})",
                    manifest.package_name,
                    req,
                    manifest
                        .features
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    // Closure over [features] items.
    while let Some(f) = work.pop() {
        let items = match manifest.features.get(&f) {
            Some(items) => items.clone(),
            None => continue, // implicit feature: no items.
        };
        for item in items {
            if let Some(dep) = item.strip_prefix("dep:") {
                if !optional.contains(dep) {
                    return Err(format!(
                        "crate '{}': feature '{}' enables 'dep:{}', which is not an optional dependency",
                        manifest.package_name, f, dep
                    ));
                }
                out.enabled_optional_deps.insert(dep.to_string());
            } else if let Some((dep, feat)) = item.split_once('/') {
                if !manifest.dependencies.iter().any(|d| d.name == dep) {
                    return Err(format!(
                        "crate '{}': feature '{}' references unknown dependency '{}'",
                        manifest.package_name, f, dep
                    ));
                }
                out.dep_features
                    .entry(dep.to_string())
                    .or_default()
                    .insert(feat.to_string());
                if optional.contains(dep) {
                    out.enabled_optional_deps.insert(dep.to_string());
                }
            } else if manifest.features.contains_key(&item) {
                enable(&mut out, &mut work, &item);
            } else if optional.contains(&item) {
                out.enabled_optional_deps.insert(item.clone());
            } else {
                return Err(format!(
                    "crate '{}': feature '{}' references unknown feature '{}'",
                    manifest.package_name, f, item
                ));
            }
        }
    }
    out.enabled.remove("default");
    Ok(out)
}

fn cfg_args_for(enabled: &BTreeSet<String>) -> Vec<String> {
    let mut args = vec![];
    for f in enabled {
        args.push("--cfg".to_string());
        args.push(format!("feature=\"{}\"", f));
    }
    args
}

// ------------------------------------------------------------ target cfg eval
struct CfgParser {
    chars: Vec<char>,
    i: usize,
}

impl CfgParser {
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\n')) {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }
    fn parse_expr(
        &mut self,
        parent_features: &BTreeSet<String>,
        warnings: &mut Vec<String>,
    ) -> Result<bool, String> {
        self.skip_ws();
        let mut ident = String::new();
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                ident.push(c);
                self.i += 1;
            } else {
                break;
            }
        }
        if ident.is_empty() {
            return Err("expected cfg atom or all/any/not".to_string());
        }
        self.skip_ws();
        match ident.as_str() {
            "all" | "any" => {
                if self.peek() != Some('(') {
                    return Err(format!("cfg {} expects '('", ident));
                }
                self.i += 1;
                let mut vals = vec![];
                loop {
                    self.skip_ws();
                    if self.peek() == Some(')') {
                        self.i += 1;
                        break;
                    }
                    vals.push(self.parse_expr(parent_features, warnings)?);
                    self.skip_ws();
                    match self.peek() {
                        Some(',') => {
                            self.i += 1;
                        }
                        Some(')') => {}
                        _ => return Err("expected ',' or ')' in cfg".to_string()),
                    }
                }
                Ok(if ident == "all" {
                    vals.iter().all(|&v| v)
                } else {
                    vals.iter().any(|&v| v)
                })
            }
            "not" => {
                if self.peek() != Some('(') {
                    return Err("cfg not expects '('".to_string());
                }
                self.i += 1;
                let v = self.parse_expr(parent_features, warnings)?;
                self.skip_ws();
                if self.peek() != Some(')') {
                    return Err("expected ')' after cfg not".to_string());
                }
                self.i += 1;
                Ok(!v)
            }
            _ => {
                // Atom: `name` or `name = "value"`.
                self.skip_ws();
                let mut value: Option<String> = None;
                if self.peek() == Some('=') {
                    self.i += 1;
                    self.skip_ws();
                    if self.peek() != Some('"') {
                        return Err("cfg atom value must be quoted".to_string());
                    }
                    self.i += 1;
                    let mut s = String::new();
                    while let Some(c) = self.peek() {
                        if c == '"' {
                            break;
                        }
                        s.push(c);
                        self.i += 1;
                    }
                    if self.peek() != Some('"') {
                        return Err("unterminated cfg string".to_string());
                    }
                    self.i += 1;
                    value = Some(s);
                }
                Ok(eval_cfg_atom(
                    &ident,
                    value.as_deref(),
                    parent_features,
                    warnings,
                ))
            }
        }
    }
}

fn host_os() -> &'static str {
    std::env::consts::OS
}
fn host_arch() -> &'static str {
    std::env::consts::ARCH
}

/// Evaluate one cfg atom on this host. Unknown atoms are false + loud.
fn eval_cfg_atom(
    name: &str,
    value: Option<&str>,
    parent_features: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> bool {
    match (name, value) {
        ("unix", None) => cfg!(unix),
        ("windows", None) => cfg!(windows),
        ("target_os", Some(v)) => v == host_os(),
        ("target_arch", Some(v)) => v == host_arch(),
        ("target_family", Some(v)) => {
            (v == "unix" && cfg!(unix)) || (v == "windows" && cfg!(windows))
        }
        ("target_pointer_width", Some(v)) => v == format!("{}", std::mem::size_of::<usize>() * 8),
        ("target_endian", Some(v)) => v == "little" || v == "big" && cfg!(target_endian = "big"),
        ("target_env", Some(v)) => {
            (host_os() == "linux" && v == "gnu") || (host_os() == "windows" && v == "msvc")
        }
        ("feature", Some(v)) => parent_features.contains(v),
        ("test", None) => false,
        ("debug_assertions", None) => cfg!(debug_assertions),
        _ => {
            warnings.push(format!(
                "unknown cfg atom '{}' treated as false (loud)",
                match value {
                    Some(v) => format!("{} = \"{}\"", name, v),
                    None => name.to_string(),
                }
            ));
            false
        }
    }
}

/// `target` is the raw section key: either `cfg(...)` or a target triple.
/// Returns None when the section is not target-gated at all.
fn target_applies(
    target: &Option<String>,
    parent_features: &BTreeSet<String>,
    warnings: &mut Vec<String>,
) -> bool {
    let t = match target {
        None => return true,
        Some(t) => t,
    };
    if let Some(inner) = t.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')')) {
        let mut p = CfgParser {
            chars: inner.chars().collect(),
            i: 0,
        };
        return p.parse_expr(parent_features, warnings).unwrap_or(false);
    }
    // Bare triple: match the host triple approximately.
    let host_triple = format!("{}-unknown-{}-gnu", host_arch(), host_os());
    t == &host_triple
        || (host_os() == "windows" && t == &format!("{}-pc-windows-msvc", host_arch()))
        || (host_os() == "macos" && t == &format!("{}-apple-darwin", host_arch()))
}

// ------------------------------------------------------------ plan building
struct Builder {
    planned: Vec<PlannedCrate>,
    index: HashMap<PathBuf, usize>,
    visiting: HashSet<PathBuf>,
    /// Per-crate request state, for feature-unification merges on revisit.
    requests: HashMap<PathBuf, (CargoManifest, FeatureOpts, BTreeMap<String, Vec<String>>)>,
    unsupported: Vec<String>,
    warnings: Vec<String>,
    excluded_target_deps: Vec<String>,
}

impl Builder {
    fn build_crate(
        &mut self,
        manifest_path: &Path,
        edge_kind: &str,
        opts: &FeatureOpts,
        child_requests: &BTreeMap<String, Vec<String>>,
    ) -> Result<(), String> {
        let manifest = parse_manifest(manifest_path)?;
        let abs_path = PathBuf::from(&manifest.manifest_path);
        if self.visiting.contains(&abs_path) {
            self.warnings.push(format!(
                "dependency cycle at '{}'; not recursing (loud)",
                manifest.package_name
            ));
            return Ok(());
        }
        if let Some(&idx) = self.index.get(&abs_path) {
            // Feature unification, partial (documented limitation): merge
            // the newly requested features into the existing entry's cfg
            // set, but do not re-expand its children.
            let (stored_manifest, stored_opts, stored_child) =
                self.requests.get(&abs_path).unwrap().clone();
            let mut merged_opts = stored_opts.clone();
            for f in &opts.features {
                if !merged_opts.features.contains(f) {
                    merged_opts.features.push(f.clone());
                }
            }
            if opts.all_features {
                merged_opts.all_features = true;
            }
            // Feature unification is additive (Cargo semantics): defaults stay
            // on unless EVERY edge disables them.
            merged_opts.no_default_features =
                stored_opts.no_default_features && opts.no_default_features;
            let mut merged_child = stored_child.clone();
            for (k, v) in child_requests {
                merged_child
                    .entry(k.clone())
                    .or_default()
                    .extend(v.iter().cloned());
            }
            match resolve_features(&stored_manifest, &merged_opts) {
                Ok(res) => {
                    let new_enabled: Vec<String> = res.enabled.iter().cloned().collect();
                    if new_enabled != self.planned[idx].enabled_features {
                        let entry = &mut self.planned[idx];
                        entry.enabled_features = new_enabled;
                        entry.cfg_args = cfg_args_for(&res.enabled);
                        self.warnings.push(format!(
                        "feature unification: crate '{}' revisited with additional features; cfg merged, child expansion not re-run (loud limitation)",
                        stored_manifest.package_name
                    ));
                    }
                    self.requests
                        .insert(abs_path, (stored_manifest, merged_opts, merged_child));
                }
                Err(e) => self.warnings.push(format!(
                    "feature unification failed for '{}': {} (kept first resolution, loud)",
                    stored_manifest.package_name, e
                )),
            }
            return Ok(());
        }
        let resolved = resolve_features(&manifest, opts)?;
        self.warnings.extend(manifest.warnings.iter().cloned());
        self.visiting.insert(abs_path.clone());
        // Recurse into buildable path deps (normal + build; dev never built).
        let mut dep_entries: Vec<Dependency> = manifest.dependencies.clone();
        for dep in dep_entries.iter_mut() {
            if dep.kind == "dev" {
                continue;
            }
            if !target_applies(&dep.target, &resolved.enabled, &mut self.warnings) {
                self.excluded_target_deps.push(format!(
                    "{} (target '{}', from {})",
                    dep.name,
                    dep.target.clone().unwrap_or_default(),
                    manifest.package_name
                ));
                continue;
            }
            if dep.workspace_inherited {
                self.unsupported.push(format!(
                    "dependency '{}' of '{}' is inherited from [workspace.dependencies] and is not resolved by stream 7",
                    dep.name, manifest.package_name
                ));
                continue;
            }
            if let Some(git) = &dep.git {
                self.unsupported.push(format!(
                    "git dependency '{}' of '{}' ({} {}) is not fetched by stream 7",
                    dep.name,
                    manifest.package_name,
                    git,
                    dep.git_ref.clone().unwrap_or_default()
                ));
                continue;
            }
            if let Some(path) = &dep.path {
                let dir = normalize_path(&PathBuf::from(&manifest.manifest_dir).join(path));
                let dep_manifest = dir.join("Cargo.toml");
                if !dep_manifest.is_file() {
                    self.unsupported.push(format!(
                        "path dependency '{}' of '{}' at {} has no Cargo.toml",
                        dep.name,
                        manifest.package_name,
                        dir.display()
                    ));
                    continue;
                }
                dep.resolved_path = Some(dir.to_string_lossy().to_string());
                let enabled_here = dep.kind == "normal" || dep.kind == "build";
                let optional_ok =
                    !dep.optional || resolved.enabled_optional_deps.contains(&dep.name);
                if !enabled_here || !optional_ok {
                    continue;
                }
                // Features requested for this dep: its own table list (plain
                // items) + parent feature items `dep/feat`, plus slash items
                // from its own list forwarded to its children.
                let mut child_opts = FeatureOpts {
                    features: vec![],
                    no_default_features: !dep.default_features,
                    all_features: false,
                };
                let mut grandchild: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for f in &dep.features {
                    if let Some((g, gf)) = f.split_once('/') {
                        grandchild
                            .entry(g.to_string())
                            .or_default()
                            .push(gf.to_string());
                    } else {
                        child_opts.features.push(f.clone());
                    }
                }
                if let Some(extra) = resolved.dep_features.get(&dep.name) {
                    for f in extra {
                        if !child_opts.features.contains(f) {
                            child_opts.features.push(f.clone());
                        }
                    }
                }
                if let Some(extra) = child_requests.get(&dep.name) {
                    for f in extra {
                        if !child_opts.features.contains(f) {
                            child_opts.features.push(f.clone());
                        }
                    }
                }
                for (k, v) in child_requests {
                    if k != &dep.name {
                        grandchild
                            .entry(k.clone())
                            .or_default()
                            .extend(v.iter().cloned());
                    }
                }
                self.build_crate(&dep_manifest, &dep.kind, &child_opts, &grandchild)?;
            } else {
                // crates.io dependency: no path, no git.
                self.unsupported.push(format!(
                    "crates.io dependency '{}' of '{}' (version req {}) is not downloaded by stream 7; use a path override or wait for crates.io support",
                    dep.name,
                    manifest.package_name,
                    dep.version.clone().unwrap_or_else(|| "*".to_string())
                ));
            }
        }
        self.visiting.remove(&abs_path);
        let planned = PlannedCrate {
            name: manifest.package_name.clone(),
            manifest_dir: manifest.manifest_dir.clone(),
            manifest_path: manifest.manifest_path.clone(),
            edition: manifest
                .edition
                .clone()
                .unwrap_or_else(|| "2015".to_string()),
            lib: manifest.lib.clone(),
            bins: manifest.bins.clone(),
            enabled_features: resolved.enabled.iter().cloned().collect(),
            cfg_args: cfg_args_for(&resolved.enabled),
            edge_kind: edge_kind.to_string(),
            dependencies: dep_entries,
        };
        self.requests.insert(
            abs_path.clone(),
            (manifest, opts.clone(), child_requests.clone()),
        );
        self.index.insert(abs_path, self.planned.len());
        self.planned.push(planned);
        Ok(())
    }
}

/// Build a plan for the crate at `input` (dir or Cargo.toml path).
pub fn build_plan(input: &Path, opts: &FeatureOpts) -> Result<BuildPlan, String> {
    let mut builder = Builder {
        planned: vec![],
        index: HashMap::new(),
        visiting: HashSet::new(),
        requests: HashMap::new(),
        unsupported: vec![],
        warnings: vec![],
        excluded_target_deps: vec![],
    };
    builder.build_crate(input, "root", opts, &BTreeMap::new())?;
    let root = builder
        .planned
        .last()
        .cloned()
        .ok_or_else(|| "no root crate planned".to_string())?;
    let root_manifest = root.manifest_path.clone();
    let root_crate = root.name.clone();
    let enabled_features = root.enabled_features.clone();
    let cfg_args = root.cfg_args.clone();
    Ok(BuildPlan {
        schema_version: 1,
        root_manifest,
        root_crate,
        crates: builder.planned,
        enabled_features,
        cfg_args,
        unsupported: builder.unsupported,
        warnings: builder.warnings,
        excluded_target_deps: builder.excluded_target_deps,
    })
}

// ------------------------------------------------------------ CLI: mir2sa cargo
pub fn run_cargo_cli(args: &[String]) -> i32 {
    let mut input: Option<String> = None;
    let mut out: Option<String> = None;
    let mut opts = FeatureOpts::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                i += 1;
                out = args.get(i).cloned();
            }
            "--features" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    for f in v.split(',') {
                        opts.features.push(f.trim().to_string());
                    }
                }
            }
            "--no-default-features" => opts.no_default_features = true,
            "--all-features" => opts.all_features = true,
            s if s.starts_with("--features=") => {
                for f in s["--features=".len()..].split(',') {
                    opts.features.push(f.trim().to_string());
                }
            }
            s if !s.starts_with("--") && input.is_none() => input = Some(s.to_string()),
            s => {
                eprintln!("cargo: unknown argument '{}'", s);
                eprintln!("usage: mir2sa cargo <manifest-dir|Cargo.toml> [--features a,b] [--no-default-features] [--all-features] [--out plan.json]");
                return 2;
            }
        }
        i += 1;
    }
    let input = match input {
        Some(v) => v,
        None => {
            eprintln!("usage: mir2sa cargo <manifest-dir|Cargo.toml> [--features a,b] [--no-default-features] [--all-features] [--out plan.json]");
            return 2;
        }
    };
    match build_plan(Path::new(&input), &opts) {
        Ok(plan) => {
            let json = serde_json::to_string_pretty(&plan).unwrap();
            match &out {
                Some(path) => {
                    if let Err(e) = std::fs::write(path, format!("{}\n", json)) {
                        eprintln!("cargo: write {}: {}", path, e);
                        return 2;
                    }
                    println!(
                        "wrote {} crates={} features=[{}] unsupported={}",
                        path,
                        plan.crates.len(),
                        plan.enabled_features.join(","),
                        plan.unsupported.len()
                    );
                }
                None => println!("{}", json),
            }
            0
        }
        Err(e) => {
            eprintln!("cargo: {}", e);
            2
        }
    }
}

// ---------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sarust-cargo-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    const BASIC: &str = r#"
# comment line
[package]
name = "demo"
version = "0.2.0"
edition = "2021"

[lib]
name = "demo_lib"
path = "src/lib.rs"

[[bin]]
name = "demo"
path = "src/main.rs"

[dependencies]
serde = "1"
foo = { path = "../foo", features = ["fast"], optional = true }

[features]
default = ["std"]
std = []
fast = ["dep:foo", "foo/derive"]

[profile.dev]
opt-level = 0
debug = true
"#;

    #[test]
    fn parse_basic_sections() {
        let dir = tmpdir("basic");
        write(&dir, "Cargo.toml", BASIC);
        let m = parse_manifest(&dir).unwrap();
        assert_eq!(m.package_name, "demo");
        assert_eq!(m.package_version.as_deref(), Some("0.2.0"));
        assert_eq!(m.edition.as_deref(), Some("2021"));
        assert_eq!(m.lib.as_ref().unwrap().name, "demo_lib");
        assert_eq!(m.bins.len(), 1);
        assert_eq!(m.bins[0].name, "demo");
        assert_eq!(m.dependencies.len(), 2);
        let foo = m.dependencies.iter().find(|d| d.name == "foo").unwrap();
        assert_eq!(foo.path.as_deref(), Some("../foo"));
        assert!(foo.optional);
        assert_eq!(foo.features, vec!["fast".to_string()]);
        assert_eq!(m.features["default"], vec!["std".to_string()]);
        assert_eq!(m.profiles["dev"]["opt-level"], "0");
        assert_eq!(m.profiles["dev"]["debug"], "true");
    }

    #[test]
    fn feature_closure_dep_and_slash() {
        let dir = tmpdir("feat");
        write(&dir, "Cargo.toml", BASIC);
        let m = parse_manifest(&dir).unwrap();
        // Default only: std enabled, foo NOT enabled (fast is not default).
        let res = resolve_features(&m, &FeatureOpts::default()).unwrap();
        assert!(res.enabled.contains("std"));
        assert!(!res.enabled_optional_deps.contains("foo"));
        // --features fast: dep:foo + foo/derive.
        let res = resolve_features(
            &m,
            &FeatureOpts {
                features: vec!["fast".to_string()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(res.enabled_optional_deps.contains("foo"));
        assert_eq!(
            res.dep_features["foo"],
            BTreeSet::from(["derive".to_string()])
        );
        assert!(cfg_args_for(&res.enabled)
            .join(" ")
            .contains("feature=\"fast\""));
    }

    #[test]
    fn feature_unknown_is_error_and_no_default_works() {
        let dir = tmpdir("unknown");
        write(&dir, "Cargo.toml", BASIC);
        let m = parse_manifest(&dir).unwrap();
        assert!(resolve_features(
            &m,
            &FeatureOpts {
                features: vec!["nope".to_string()],
                ..Default::default()
            }
        )
        .is_err());
        let res = resolve_features(
            &m,
            &FeatureOpts {
                no_default_features: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!res.enabled.contains("std"));
        assert!(res.enabled.is_empty());
    }

    #[test]
    fn plan_path_dep_order_and_crates_io_loud() {
        let dir = tmpdir("plan");
        write(
            &dir,
            "foo/Cargo.toml",
            "[package]\nname = \"foo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write(
            &dir,
            "foo/src/lib.rs",
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        );
        write(
            &dir,
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nfoo = { path = \"../foo\" }\nserde = \"1\"\n",
        );
        write(
            &dir,
            "app/src/main.rs",
            "fn main() { let _v = foo::add(1, 2); }\n",
        );
        let plan = build_plan(&dir.join("app"), &FeatureOpts::default()).unwrap();
        assert_eq!(plan.crates.len(), 2);
        assert_eq!(plan.crates[0].name, "foo");
        assert_eq!(plan.crates[1].name, "app");
        assert_eq!(plan.root_crate, "app");
        let foo_dep = plan.crates[1]
            .dependencies
            .iter()
            .find(|d| d.name == "foo")
            .unwrap();
        assert!(foo_dep.resolved_path.as_ref().unwrap().ends_with("foo"));
        assert_eq!(plan.unsupported.len(), 1);
        assert!(plan.unsupported[0].contains("crates.io"));
        assert!(plan.unsupported[0].contains("serde"));
    }

    #[test]
    fn plan_workspace_inherited_is_loud() {
        let dir = tmpdir("ws");
        write(
            &dir,
            "Cargo.toml",
            "[package]\nname = \"w\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { workspace = true }\n",
        );
        let plan = build_plan(&dir, &FeatureOpts::default()).unwrap();
        assert!(plan.unsupported.iter().any(|u| u.contains("workspace")));
        assert_eq!(plan.crates.len(), 1);
    }

    #[test]
    fn target_cfg_eval() {
        let mut warnings = vec![];
        let feats = BTreeSet::new();
        assert!(target_applies(
            &Some("cfg(unix)".to_string()),
            &feats,
            &mut warnings
        ));
        assert!(!target_applies(
            &Some("cfg(windows)".to_string()),
            &feats,
            &mut warnings
        ));
        assert!(target_applies(
            &Some("cfg(all(unix, not(windows)))".to_string()),
            &feats,
            &mut warnings
        ));
        assert!(target_applies(
            &Some("cfg(any(windows, unix))".to_string()),
            &feats,
            &mut warnings
        ));
        assert!(!target_applies(
            &Some("cfg(target_os = \"windows\")".to_string()),
            &feats,
            &mut warnings
        ));
        assert!(target_applies(&None, &feats, &mut warnings));
        // Unknown atom: false + loud warning.
        assert!(!target_applies(
            &Some("cfg(target_abi = \"eabihf\")".to_string()),
            &feats,
            &mut warnings
        ));
        assert!(!warnings.is_empty());
    }

    #[test]
    fn multiline_array_and_table_form_dep() {
        let dir = tmpdir("multi");
        write(
            &dir,
            "Cargo.toml",
            "[package]\nname = \"m\"\nversion = \"0.1.0\"\n\n[features]\nall = [\n  \"a\",\n  \"b\", # trailing comment\n]\na = []\nb = []\n\n[dependencies.bar]\nversion = \"2\"\ndefault-features = false\n",
        );
        let m = parse_manifest(&dir).unwrap();
        assert_eq!(m.features["all"], vec!["a".to_string(), "b".to_string()]);
        let bar = m.dependencies.iter().find(|d| d.name == "bar").unwrap();
        assert_eq!(bar.version.as_deref(), Some("2"));
        assert!(!bar.default_features);
    }
}
