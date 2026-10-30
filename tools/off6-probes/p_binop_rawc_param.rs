struct P { a: u64, b: u64, c: u64 }
#[repr(C)]
struct H { r: *const P, t: u64 }
fn probe(h: H, q: *const P) -> u64 { if h.r == q { 33 } else { 0 } }
fn main() { let p = P { a: 11, b: 22, c: 33 }; let h = H { r: &p as *const P, t: 7 }; std::process::exit(probe(h, &p as *const P) as i32); }
