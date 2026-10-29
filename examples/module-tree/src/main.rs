// Multi-file mod tree: two modules define the same fn name (`compute`),
// plus a plain module and a nested submodule. Single-arg println! calls
// only (two-arg println! is a pre-existing mir2sa TRAP, unrelated here).
mod m1;
mod m2;
mod util;

fn main() {
    let a = m1::compute(3);
    println!("{}", a);
    let b = m2::compute(4);
    println!("{}", b);
    let c = util::double(21);
    println!("{}", c);
    let d = util::inner::triple(5);
    println!("{}", d);
}
