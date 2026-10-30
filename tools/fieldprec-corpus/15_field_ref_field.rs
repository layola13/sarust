struct P { a: u64, b: u64, c: u64 }
fn probe() -> u64 { let p = P { a: 11, b: 22, c: 33 }; let r = &p; r.c }
fn main() { std::process::exit(probe() as i32); }
