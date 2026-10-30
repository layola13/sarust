struct P { a: u64, b: u64, c: u64 }
fn probe() -> u64 { let p = P { a: 11, b: 22, c: 33 }; let q = P { a: 5, ..p }; q.b }
fn main() { std::process::exit(probe() as i32); }
