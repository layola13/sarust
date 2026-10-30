struct P { a: u64, b: u64, c: u64 }
fn id(x: u64) -> u64 { x }
fn probe() -> u64 { let p = P { a: 11, b: 22, c: 33 }; id(p.c) }
fn main() { std::process::exit(probe() as i32); }
