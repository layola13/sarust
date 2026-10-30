fn id(x: u64) -> u64 { x }
fn probe() -> u64 { let a = [11u64, 22, 33]; id(a[2]) }
fn main() { std::process::exit(probe() as i32); }
