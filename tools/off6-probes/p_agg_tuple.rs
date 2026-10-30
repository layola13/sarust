struct P { a: u64, b: u64, c: u64 }
struct H<'a> { t: u64, r: &'a P }
fn probe() -> u64 { let p = P { a: 11, b: 22, c: 33 }; let h = H { t: 7, r: &p }; let t = (h.r, 9u64); t.0.c }
fn main() { std::process::exit(probe() as i32); }
