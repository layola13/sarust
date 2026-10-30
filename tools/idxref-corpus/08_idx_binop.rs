fn probe() -> u64 { let a = [11u64, 22, 33]; a[0] + a[2] }
fn main() { std::process::exit(probe() as i32); }
