fn probe() -> u64 { let a = [11u64, 22, 33]; let r = &a; r[2] }
fn main() { std::process::exit(probe() as i32); }
