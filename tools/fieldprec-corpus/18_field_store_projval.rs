struct P { a: u64, b: u64, c: u64 }
fn probe() -> u64 { let p = P { a: 11, b: 22, c: 33 }; let mut q = P { a: 1, b: 2, c: 3 }; q.b = p.c; q.a + q.b + q.c }
fn main() { std::process::exit(probe() as i32); }
