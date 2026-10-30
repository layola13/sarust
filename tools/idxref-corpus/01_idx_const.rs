fn probe() -> u64 { let a = [11u64, 22, 33]; a[1] }
fn main() { std::process::exit(probe() as i32); }
