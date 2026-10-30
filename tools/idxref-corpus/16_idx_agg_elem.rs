struct S { x: u64, y: u64 }
fn probe() -> u64 { let a = [11u64, 22, 33]; let s = S { x: a[2], y: 1 }; s.x + s.y }
fn main() { std::process::exit(probe() as i32); }
