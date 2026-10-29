// Bin crate: calls lib fns (triple, helper) and defines its own local
// `helper`, colliding with mylib::helper across crates. Single-arg
// println! calls only (two-arg println! is a pre-existing mir2sa TRAP).
use mylib::{helper as lib_helper, triple};

fn helper(n: i32) -> i32 {
    n + 1
}

fn main() {
    let t = triple(14);
    println!("{}", t);
    let h1 = lib_helper(1);
    println!("{}", h1);
    let h2 = helper(1);
    println!("{}", h2);
}
