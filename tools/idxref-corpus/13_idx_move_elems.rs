fn probe() -> u64 { let x = 11u64 + 1; let a = [x, 22, 33]; a[0] + a[1] }
fn main() { std::process::exit(probe() as i32); }
