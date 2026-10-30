struct P { a: u64, b: u64, c: u64 }
struct H<'a> { t: u64, r: &'a P }
struct Q<'a> { r: &'a P, v: u64 }
fn mk(p: &P) -> H { H { t: 7, r: p } }
fn probe() -> u64 { let p = P { a: 11, b: 22, c: 33 }; let h = mk(&p); let q = Q { r: h.r, v: 9 }; q.r.c }
fn main() { std::process::exit(probe() as i32); }
