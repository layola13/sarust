struct P { a: u64, b: u64, c: u64 }
fn probe() -> u64 { let flag = 1u64 + 1 == 2; let q = if flag { P { a: 11, b: 22, c: 33 } } else { P { a: 44, b: 55, c: 66 } }; q.b }
fn main() { std::process::exit(probe() as i32); }
