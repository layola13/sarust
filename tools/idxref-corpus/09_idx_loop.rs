fn probe() -> u64 { let a = [11u64, 22, 33]; let mut s = 0u64; let mut i = 0usize; while i < 3 { s += a[i]; i += 1; } s }
fn main() { std::process::exit(probe() as i32); }
