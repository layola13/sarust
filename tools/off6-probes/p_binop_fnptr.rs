#[repr(C)]
struct H { f: fn() -> u64, t: u64 }
fn forty_two() -> u64 { 42 }
fn probe() -> u64 { let h = H { f: forty_two, t: 7 }; if h.f == forty_two { 33 } else { 0 } }
fn main() { std::process::exit(probe() as i32); }
