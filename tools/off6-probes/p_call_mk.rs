struct P { a: u64, b: u64, c: u64 }
struct H<'a> { t: u64, r: &'a P }
fn mk(p: &P) -> H { H { t: 7, r: p } }
fn idp(x: &P) -> u64 { x.c }
fn probe() -> u64 { let p = P { a: 11, b: 22, c: 33 }; let h = mk(&p); idp(h.r) }
fn main() { std::process::exit(probe() as i32); }
