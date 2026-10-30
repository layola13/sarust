struct P { a: u64, b: u64, c: u64 }
fn get_b(p: P) -> u64 { p.b }
fn probe() -> u64 { let p = P { a: 11, b: 22, c: 33 }; get_b(p) }
fn main() { std::process::exit(probe() as i32); }
