struct P { a: u64, b: u64, c: u64 }
struct H<'a> { t: u64, r: &'a P }
fn idp(x: &P) -> u64 { x.c }
fn probe(h: H) -> u64 { idp(h.r) }
fn main() { let p = P { a: 11, b: 22, c: 33 }; let h = H { t: 7, r: &p }; std::process::exit(probe(h) as i32); }
