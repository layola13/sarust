//! rsc_driver: real rustc_private driver — `.rs -> MIR -> mir.json`.
//!
//! Official compiler does 100% checking (parse/resolve/hir/typeck/borrowck);
//! we only hijack the resulting MIR in `after_analysis` and dump the stable
//! mir.json subset consumed by `mir2sa lower`. No codegen runs
//! (`Compilation::Stop`). If borrowck/typeck reported errors, no mir.json is
//! written (zero-error backend input, guaranteed).
//!
//! Build (needs nightly + rustc-dev, both present):
//!   ./build.sh
//! Run:
//!   ./target/rsc_driver --edition 2021 /tmp/rsc_demo/src/main.rs \
//!       --rsc-out /tmp/driver.mir.json --crate-type bin

#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_ast;
extern crate rustc_abi;
extern crate rustc_span;

use rustc_driver::{Callbacks, Compilation, run_compiler};
use rustc_hir::def::DefKind;
use rustc_interface::interface::Compiler;
use rustc_middle::ty::TyCtxt;

use crate::emit::body_json;

struct RscCallbacks {
    out: String,
}

mod emit;
mod place;
mod tyinfo;
mod util;

impl Callbacks for RscCallbacks {
    fn after_analysis<'tcx>(&mut self, _compiler: &Compiler, tcx: TyCtxt<'tcx>) -> Compilation {
        if tcx.dcx().err_count() > 0 {
            eprintln!("rsc_driver: analysis had errors; refusing to emit mir.json");
            std::process::exit(1);
        }
        let mut out = String::from("{\"source\": \"rsc_driver (rustc_private, real MIR)\", \"functions\": [");
        let mut first = true;
        for owner in tcx.hir_body_owners() {
            let did = owner.to_def_id();
            // `optimized_mir` panics on constants ("do not use optimized_mir
            // for constants"): route consts/statics through `mir_for_ctfe`,
            // skip bodies with no MIR at all.
            let body = match tcx.def_kind(did) {
                DefKind::Const | DefKind::AssocConst | DefKind::AnonConst | DefKind::Static { .. } => {
                    tcx.mir_for_ctfe(did)
                }
                DefKind::GlobalAsm => continue,
                _ => tcx.optimized_mir(did),
            };
            let path = tcx.def_path_str(did);
            let short = path.rsplit("::").next().unwrap_or(&path);
            if !first {
                out.push_str(", ");
            }
            first = false;
            body_json(tcx, tcx.param_env(did), short, body, &mut out);
        }
        out.push_str("]}");
        let approx = out.matches("RuntimeChecks-unsupported").count();
        if let Err(e) = std::fs::write(&self.out, &out) {
            eprintln!("rsc_driver: write {}: {}", self.out, e);
            std::process::exit(1);
        }
        eprintln!("rsc_driver: wrote {} APPROX={}", self.out, approx);
        Compilation::Stop
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().collect();
    let mut out = String::from("out.mir.json");
    let mut rustc_args: Vec<String> = vec![raw.get(0).cloned().unwrap_or_else(|| "rsc_driver".to_string())];
    let mut i = 1;
    while i < raw.len() {
        if raw[i] == "--rsc-out" {
            i += 1;
            if let Some(v) = raw.get(i) {
                out = v.clone();
            }
        } else {
            rustc_args.push(raw[i].clone());
        }
        i += 1;
    }
    let mut cb = RscCallbacks { out };
    run_compiler(&rustc_args, &mut cb);
}
