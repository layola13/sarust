struct Inner { x: u64, y: u64 }
struct Outer { v: u64, inner: Inner }
fn probe() -> u64 { let o = Outer { v: 11, inner: Inner { x: 77, y: 88 } }; let inn = o.inner; inn.y }
fn main() { std::process::exit(probe() as i32); }
