struct P { a: u64, b: u64, c: u64 }
fn probe() -> u64 { let mut p = P { a: 11, b: 22, c: 33 }; p.b = 99; p.a + p.b + p.c }
fn main() { std::process::exit(probe() as i32); }
