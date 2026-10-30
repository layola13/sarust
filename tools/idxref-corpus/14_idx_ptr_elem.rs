struct P { a: u64, b: u64 }
fn probe() -> u64 { let p = P { a: 11, b: 22 }; let a = [&p.a, &p.b]; *a[1] }
fn main() { std::process::exit(probe() as i32); }
