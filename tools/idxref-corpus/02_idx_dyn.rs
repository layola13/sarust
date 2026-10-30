fn probe() -> u64 { let a = [11u64, 22, 33]; let i = 1 + 1; a[i] }
fn main() { std::process::exit(probe() as i32); }
