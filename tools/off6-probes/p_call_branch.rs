struct P { a: u64, b: u64, c: u64 }
struct H<'a> { t: u64, r: &'a P }
fn idp(x: &P) -> u64 { x.c }
fn probe(c: bool) -> u64 { let p = P { a: 11, b: 22, c: 33 }; let h = if c { H { t: 7, r: &p } } else { H { t: 8, r: &p } }; idp(h.r) }
fn main() { std::process::exit(probe(true) as i32); }
