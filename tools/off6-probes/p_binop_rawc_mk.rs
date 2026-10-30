struct P { a: u64, b: u64, c: u64 }
#[repr(C)]
struct H { r: *const P, t: u64 }
fn mk(q: *const P) -> H { H { r: q, t: 7 } }
fn probe() -> u64 { let p = P { a: 11, b: 22, c: 33 }; let h = mk(&p as *const P); if h.r == &p as *const P { 33 } else { 0 } }
fn main() { std::process::exit(probe() as i32); }
