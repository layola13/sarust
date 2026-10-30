struct P { a: u64, b: u64, c: u64 }
fn id(x: u64) -> u64 { x }
fn probe() -> u64 { let mut p = P { a: 11, b: 22, c: 33 }; let v = id(99); p.b = v; p.b }
fn main() { std::process::exit(probe() as i32); }
